#!/usr/bin/env bash
# Builds the plugin and assembles target/bundled/chopsuey.{vst3,clap},
# ready to load: the stem helper (once the stems_helper crate exists), the
# libraries it or the plugin links (Dawn), the stem model, proper
# Info.plists, and an ad hoc signature.
#
#   scripts/bundle.sh [--universal] [--no-model] [--rt-checks]
#
#   --universal  arm64 + x86_64 (needs `rustup target add x86_64-apple-darwin`)
#   --no-model   leave the 166 MB stem model out (CI, validation); stem
#                separation is then unavailable in that build
#   --rt-checks  a debug build with the `rt-checks` feature (panics on any
#                allocation in process()), into target/bundled/rt-checks/,
#                for validation only. Implies --no-model.
#
# Signing for distribution is scripts/release.sh's job; this always signs
# ad hoc so the bundles load locally.
source "$(dirname "$0")/common.sh"

UNIVERSAL=0 WITH_MODEL=1 RT_CHECKS=0
for arg in "$@"; do
  case "$arg" in
    --universal) UNIVERSAL=1 ;;
    --no-model) WITH_MODEL=0 ;;
    --rt-checks) RT_CHECKS=1 WITH_MODEL=0 ;;
    *) die "unknown option $arg" ;;
  esac
done

PROFILE=release PROFILE_ARGS=(--release) FEATURE_ARGS=()
if (( RT_CHECKS )); then
  PROFILE=debug PROFILE_ARGS=()
  if grep -q '^rt-checks *=' Cargo.toml; then
    FEATURE_ARGS=(--features rt-checks)
  else
    warn "Cargo.toml has no rt-checks feature yet: building plain debug (no allocation checks)"
  fi
fi
# "${a[@]+...}" keeps empty arrays working under `set -u` in macOS's bash 3.2.
PLUGIN_ARGS=(${PROFILE_ARGS[@]+"${PROFILE_ARGS[@]}"} ${FEATURE_ARGS[@]+"${FEATURE_ARGS[@]}"})

TRIPLES=(aarch64-apple-darwin x86_64-apple-darwin)
if (( UNIVERSAL )); then
  for t in "${TRIPLES[@]}"; do
    rustup target list --installed | grep -qx "$t" || die "missing Rust target: run 'rustup target add $t'"
  done
fi

# Where cargo put the build for a triple (native builds have no triple dir).
out_dir() { if (( UNIVERSAL )); then echo "$TARGET_DIR/$1/$PROFILE"; else echo "$TARGET_DIR/$PROFILE"; fi; }

step "Building the plugin ($PROFILE)"
# nih_plug_xtask always writes to target/bundled: keep the release bundles
# out of the way of an rt-checks build.
SAVED="$TARGET_DIR/bundled-saved"
rm -rf "$SAVED"
if (( RT_CHECKS )); then
  mkdir -p "$SAVED"
  for f in "$BUNDLED/$NAME.vst3" "$BUNDLED/$NAME.clap"; do
    [[ -e "$f" ]] && mv "$f" "$SAVED/"
  done
fi
if (( UNIVERSAL )); then
  cargo xtask bundle-universal "$PACKAGE" ${PLUGIN_ARGS[@]+"${PLUGIN_ARGS[@]}"}
else
  cargo xtask bundle "$PACKAGE" ${PLUGIN_ARGS[@]+"${PLUGIN_ARGS[@]}"}
fi
OUT="$BUNDLED"
if (( RT_CHECKS )); then
  OUT="$BUNDLED/rt-checks"
  rm -rf "$OUT" && mkdir -p "$OUT"
  mv "$BUNDLED/$NAME.vst3" "$BUNDLED/$NAME.clap" "$OUT/"
  if [[ -d "$SAVED" ]]; then mv "$SAVED"/* "$BUNDLED/" 2>/dev/null || true; rm -rf "$SAVED"; fi
fi
BUNDLES=("$OUT/$NAME.vst3" "$OUT/$NAME.clap")

# The helper: per architecture, lipo'd when there are two. ONNX Runtime
# has no prebuilt x86_64 macOS binaries (see README), so if the Intel build
# fails the helper ships arm64-only: the plugin itself still runs on Intel,
# without stem separation.
HELPER=""
if has_helper; then
  step "Building the stem helper ($HELPER_BIN)"
  if (( UNIVERSAL )); then
    slices=()
    for t in "${TRIPLES[@]}"; do
      if cargo build ${PROFILE_ARGS[@]+"${PROFILE_ARGS[@]}"} -p "$HELPER_CRATE" --target "$t"; then
        slices+=("$(out_dir "$t")/$HELPER_BIN")
      elif [[ "$t" == aarch64-* ]]; then
        die "the arm64 helper didn't build"
      else
        warn "the $t helper didn't build: shipping an arm64-only helper (no stem separation on Intel Macs)"
      fi
    done
    HELPER="$TARGET_DIR/universal-$PROFILE/$HELPER_BIN"
    mkdir -p "$(dirname "$HELPER")"
    lipo -create "${slices[@]}" -output "$HELPER"
  else
    cargo build ${PROFILE_ARGS[@]+"${PROFILE_ARGS[@]}"} -p "$HELPER_CRATE"
    HELPER="$(out_dir native)/$HELPER_BIN"
  fi
fi

# Copies every @rpath library that the binaries in Contents/MacOS link into
# Contents/MacOS (build.rs sets @rpath = @loader_path). Today that is Dawn
# (libwebgpu_dawn.dylib, built by ort's webgpu feature), linked by the
# plugin before the helper split and by the helper after it. Libraries that
# exist for both architectures are lipo'd; ones built for one architecture
# only (Dawn is arm64-only) are copied as they are.
copy_rpath_libs() {
  local macos="$1/Contents/MacOS" bin lib found
  local -a srcs
  for bin in "$macos"/*; do
    file -b "$bin" | grep -q Mach-O || continue
    while read -r lib; do
      [[ -n "$lib" && ! -e "$macos/$lib" ]] || continue
      srcs=()
      if (( UNIVERSAL )); then
        for t in "${TRIPLES[@]}"; do
          found="$(out_dir "$t")/$lib"
          [[ -e "$found" ]] && srcs+=("$found")
        done
      else
        found="$(out_dir native)/$lib"
        [[ -e "$found" ]] && srcs+=("$found")
      fi
      (( ${#srcs[@]} )) || die "$(basename "$bin") links @rpath/$lib, but it isn't in the build output"
      if (( ${#srcs[@]} > 1 )); then
        lipo -create "${srcs[@]}" -output "$macos/$lib"
      else
        cp -L "${srcs[0]}" "$macos/$lib"
      fi
      chmod 755 "$macos/$lib"
    done < <(otool -L "$bin" | sed -n 's|^[[:space:]]*@rpath/\([^ ]*\) .*|\1|p' | sort -u)
  done
}

# Bundle metadata. nih_plug_xtask writes a generic Info.plist
# (com.nih-plug.<crate>, version 1.0.0); replace the parts hosts and the
# installer care about.
fix_info_plist() {
  local plist="$1/Contents/Info.plist" ext="${1##*.}"
  set_key() { plutil -replace "$1" -string "$2" "$plist"; }
  set_key CFBundleIdentifier "$ID_PREFIX.$NAME.$ext"
  set_key CFBundleName "$NAME"
  set_key CFBundleDisplayName "$NAME"
  set_key CFBundleShortVersionString "$VERSION"
  set_key CFBundleVersion "$VERSION"
  set_key CFBundleGetInfoString "$NAME $VERSION, $VENDOR"
  set_key NSHumanReadableCopyright "Copyright © $(date +%Y) $VENDOR. All rights reserved."
  # The newest minimum of anything inside (Dawn and ONNX Runtime's prebuilt
  # binaries need 13.4), not just the plugin binary's.
  set_key LSMinimumSystemVersion "$(bundle_min_macos "$1")"
  plutil -lint -s "$plist"
}

step "Assembling bundles"
for b in "${BUNDLES[@]}"; do
  [[ -d "$b" ]] || die "nih_plug_xtask didn't produce $b"
  rm -rf "$b/Contents/Resources" "$b/Contents/MacOS/$HELPER_BIN"
  find "$b/Contents/MacOS" -name '*.dylib' -delete
  if [[ -n "$HELPER" ]]; then
    cp "$HELPER" "$b/Contents/MacOS/$HELPER_BIN"
  fi
  copy_rpath_libs "$b"
  # The linker records the build directory as the dylib's install name; no
  # one links against a plugin, so don't ship that path.
  install_name_tool -id "@rpath/$NAME" "$b/Contents/MacOS/$NAME" 2>/dev/null
  if (( WITH_MODEL )); then
    fetch_model
    mkdir -p "$b/Contents/Resources"
    # A clone on APFS: no extra disk space for the second bundle.
    cp -c "$MODEL" "$b/Contents/Resources/htdemucs.onnx" 2>/dev/null \
      || cp "$MODEL" "$b/Contents/Resources/htdemucs.onnx"
  fi
  fix_info_plist "$b"
done
(( WITH_MODEL )) || warn "bundled without the stem model: stem separation is unavailable in this build"

DEVELOPER_ID_APP="" scripts/sign.sh "${BUNDLES[@]}"
for b in "${BUNDLES[@]}"; do
  echo "$b ($(lipo -archs "$b/Contents/MacOS/$NAME"))"
done
