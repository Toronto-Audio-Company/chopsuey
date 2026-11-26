#!/usr/bin/env bash
# Builds the release installer: target/dist/chopsuey-<version>.pkg, which
# installs the VST3, CLAP and AU to /Library/Audio/Plug-Ins/{VST3,CLAP,
# Components}, plus target/dist/uninstall-chopsuey.sh to ship alongside.
#
#   scripts/release.sh [--native]
#
#   --native  this Mac's architecture only (default: arm64 + x86_64)
#
# Signing and notarization come from the environment:
#   DEVELOPER_ID_APP        "Developer ID Application: <Name> (<TEAMID>)"
#   DEVELOPER_ID_INSTALLER  "Developer ID Installer: <Name> (<TEAMID>)"
#   NOTARY_PROFILE          a keychain profile made with
#                           `xcrun notarytool store-credentials <name>`
# Without them the code is signed ad hoc, the .pkg is unsigned and nothing
# is notarized: fine for testing on this Mac, not for distribution.
source "$(dirname "$0")/common.sh"

UNIVERSAL=1
for arg in "$@"; do
  case "$arg" in
    --native) UNIVERSAL=0 ;;
    *) die "unknown option $arg" ;;
  esac
done

APP_ID="${DEVELOPER_ID_APP:-}"
INSTALLER_ID="${DEVELOPER_ID_INSTALLER:-}"
NOTARY="${NOTARY_PROFILE:-}"
DIST="$TARGET_DIR/dist"
STAGE="$TARGET_DIR/pkg-stage"
PKG="$DIST/$NAME-$VERSION.pkg"

if [[ -z "$APP_ID" ]]; then
  warn "DEVELOPER_ID_APP is not set: signing ad hoc. This build only runs on Macs that built it or that allow it by hand."
fi
if [[ -z "$INSTALLER_ID" ]]; then
  warn "DEVELOPER_ID_INSTALLER is not set: the .pkg will be unsigned."
fi
CAN_NOTARIZE=0
if [[ -n "$APP_ID" && -n "$INSTALLER_ID" && -n "$NOTARY" ]]; then
  CAN_NOTARIZE=1
else
  warn "notarization skipped: it needs DEVELOPER_ID_APP, DEVELOPER_ID_INSTALLER and NOTARY_PROFILE."
fi

fetch_model
# Keep this machine's paths (home directory, registry) out of the shipped
# binaries' panic messages.
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$HOME=~ --remap-path-prefix=$ROOT=chopsuey"

if (( UNIVERSAL )); then scripts/bundle.sh --universal; else scripts/bundle.sh; fi
packaging/au/build.sh

step "Signing"
BUNDLES=("$BUNDLED/$NAME.vst3" "$BUNDLED/$NAME.clap" "$BUNDLED/$NAME.component")
scripts/sign.sh "${BUNDLES[@]}"
if [[ -n "$APP_ID" ]]; then
  for b in "${BUNDLES[@]}"; do
    info="$(codesign --display --verbose=2 "$b" 2>&1)"
    for want in '^Authority=Developer ID Application' '^Timestamp=' '^Runtime Version='; do
      grep -q "$want" <<<"$info" || die "$b: no '${want#^}' in its signature (needs Developer ID, timestamp, hardened runtime)"
    done
  done
fi

step "Building the installer"
rm -rf "$STAGE" "$DIST" && mkdir -p "$STAGE" "$DIST"
# One component package per format, so people can untick ones they don't
# want (Customize in the installer).
component_pkg() { # <bundle> <install dir> <id suffix> [scripts dir]
  local bundle="$1" dir="$2" id="$PKG_ID.$3" root="$STAGE/root-$3" plist="$STAGE/$3.plist"
  mkdir -p "$root"
  ditto "$bundle" "$root/$(basename "$bundle")"
  pkgbuild --analyze --root "$root" "$plist" >/dev/null
  # Never "relocate" to wherever a bundle with the same identifier happens
  # to be (a developer build, a copy on another drive): always install here.
  plutil -replace 0.BundleIsRelocatable -bool NO "$plist"
  pkgbuild --root "$root" --component-plist "$plist" --identifier "$id" --version "$VERSION" \
    --install-location "$dir" ${4:+--scripts "$4"} "$STAGE/$NAME-$3.pkg" >/dev/null
  rm -rf "$root"
}
component_pkg "$BUNDLED/$NAME.vst3" /Library/Audio/Plug-Ins/VST3 vst3
component_pkg "$BUNDLED/$NAME.clap" /Library/Audio/Plug-Ins/CLAP clap
component_pkg "$BUNDLED/$NAME.component" /Library/Audio/Plug-Ins/Components au "$ROOT/packaging/pkg/scripts-au"

MIN_OS="$(bundle_min_macos "${BUNDLES[@]}")"
HOST_ARCHS="$(lipo -archs "$BUNDLED/$NAME.vst3/Contents/MacOS/$NAME" | tr ' ' ',')"
sed -e "s/@VERSION@/$VERSION/g" -e "s/@MIN_MACOS@/$MIN_OS/g" -e "s/@HOST_ARCHS@/$HOST_ARCHS/g" \
  packaging/pkg/distribution.xml > "$STAGE/distribution.xml"
productbuild --distribution "$STAGE/distribution.xml" --package-path "$STAGE" \
  ${INSTALLER_ID:+--sign "$INSTALLER_ID" --timestamp} "$PKG"
rm -f "$STAGE"/*.pkg
cp packaging/pkg/uninstall.sh "$DIST/uninstall-$NAME.sh"
chmod 755 "$DIST/uninstall-$NAME.sh"

if (( CAN_NOTARIZE )); then
  step "Notarizing (this usually takes a few minutes)"
  xcrun notarytool submit "$PKG" --keychain-profile "$NOTARY" --wait --timeout 1h \
    | tee "$STAGE/notary.log"
  grep -q 'status: Accepted' "$STAGE/notary.log" || {
    id="$(sed -n 's/^ *id: //p' "$STAGE/notary.log" | head -1)"
    [[ -n "$id" ]] && xcrun notarytool log "$id" --keychain-profile "$NOTARY" || true
    die "notarization failed"
  }
  xcrun stapler staple "$PKG"
  xcrun stapler validate "$PKG"
  spctl --assess --type install --verbose=2 "$PKG"
fi

step "Done"
pkgutil --check-signature "$PKG" 2>/dev/null | head -3 || true
echo "$PKG ($(du -h "$PKG" | cut -f1), macOS $MIN_OS+, $HOST_ARCHS)"
echo "$DIST/uninstall-$NAME.sh"
