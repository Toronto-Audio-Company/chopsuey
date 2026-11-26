#!/usr/bin/env bash
# Builds target/bundled/chopsuey.component (Audio Unit v2 instrument, type
# aumu, subtype Chsy, manufacturer TAud) by wrapping the CLAP build with
# free-audio/clap-wrapper. Run scripts/bundle.sh first.
#
#   packaging/au/build.sh [path/to/chopsuey.clap]
#
# Needs cmake (brew install cmake). The first run downloads clap-wrapper,
# the CLAP SDK and Apple's AudioUnitSDK into target/au-build.
source "$(dirname "$0")/../../scripts/common.sh"

CLAP="${1:-$BUNDLED/$NAME.clap}"
CLAP="$(cd "$(dirname "$CLAP")" && pwd)/$(basename "$CLAP")"
[[ -d "$CLAP" ]] || die "no CLAP at $CLAP: run scripts/bundle.sh first"
command -v cmake >/dev/null || die "cmake is needed for the AU (brew install cmake)"

OUT_DIR="$(dirname "$CLAP")"
BUILD="$TARGET_DIR/au-build"
# Build the wrapper for the same architectures as the CLAP inside it.
ARCHS="$(lipo -archs "$CLAP/Contents/MacOS/$NAME" | tr ' ' ';')"

step "Building the AU wrapper ($ARCHS)"
cmake -S "$ROOT/packaging/au" -B "$BUILD" -DCMAKE_BUILD_TYPE=Release \
  -DCHOPSUEY_CLAP="$CLAP" -DCHOPSUEY_VERSION="$VERSION" \
  -DCMAKE_OSX_ARCHITECTURES="$ARCHS" -DCMAKE_OSX_DEPLOYMENT_TARGET="$MIN_MACOS" >/dev/null
# The wrapper copies the .clap in before linking; remove the old copy so a
# rebuilt CLAP always replaces it.
rm -rf "$BUILD/$NAME.component"
cmake --build "$BUILD" --config Release -j "$(sysctl -n hw.ncpu)" >/dev/null

COMPONENT="$BUILD/$NAME.component"
[[ -d "$COMPONENT" ]] || COMPONENT="$(find "$BUILD" -maxdepth 3 -name "$NAME.component" -type d | head -1)"
[[ -d "$COMPONENT" ]] || die "the AU build produced no $NAME.component"
rm -rf "$OUT_DIR/$NAME.component"
cp -R "$COMPONENT" "$OUT_DIR/"
COMPONENT="$OUT_DIR/$NAME.component"

# Same metadata conventions as the other bundles.
plist="$COMPONENT/Contents/Info.plist"
# clap-wrapper's generated plist drops the ".component" suffix; keep each
# format's identifier distinct (and distinct from the installer's).
plutil -replace CFBundleIdentifier -string "$ID_PREFIX.$NAME.component" "$plist"
plutil -replace NSHumanReadableCopyright -string "Copyright © $(date +%Y) $VENDOR. All rights reserved." "$plist"
plutil -replace LSMinimumSystemVersion -string "$(bundle_min_macos "$COMPONENT")" "$plist"

DEVELOPER_ID_APP="" "$ROOT/scripts/sign.sh" "$COMPONENT"
echo "$COMPONENT ($(lipo -archs "$COMPONENT/Contents/MacOS/$NAME"))"
