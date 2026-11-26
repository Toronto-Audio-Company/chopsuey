#!/usr/bin/env bash
# Code-signs plugin bundles inside-out: nested bundles (the CLAP inside the
# AU) first, then loose dylibs (Dawn), then executables (the stem helper),
# then the bundle itself. No --deep: Apple advises against it for signing.
#
#   scripts/sign.sh <bundle or folder>...
#
# A plain folder (the installer's stem splitter) has its Mach-O files signed,
# libraries before executables.
#
# With DEVELOPER_ID_APP set (e.g. "Developer ID Application: Name (TEAMID)")
# everything gets that identity, the hardened runtime and a secure timestamp,
# as notarization requires. Without it, everything is signed ad hoc, which
# is enough to load on this Mac (and in CI) but not on anyone else's.
#
# Entitlements: none. They only apply to executables, and the one executable
# we ship, the stem helper, needs none:
#  - com.apple.security.cs.allow-jit: ONNX Runtime's CPU kernels (MLAS) are
#    precompiled, CoreML compiles models inside Apple's own frameworks, and
#    Dawn hands shaders to Metal, whose compiler runs in its own process. None
#    of them map writable+executable memory in our process.
#  - com.apple.security.cs.disable-library-validation: the helper only loads
#    Dawn (and the plugin is loaded by the host, whose entitlements apply),
#    and Dawn is re-signed here with the same Team ID, which is exactly what
#    library validation checks.
# Checked (Nov 2026, ort 2.0.0-rc.13): examples/split_stems separated a
# song on WebGPU and on CPU under the hardened runtime without allow-jit.
# Ad hoc signatures have no Team ID, so with the hardened runtime dyld
# refuses Dawn ("different Team IDs"): that's why the ad hoc path leaves
# the hardened runtime off.
source "$(dirname "$0")/common.sh"

IDENTITY="${DEVELOPER_ID_APP:-}"
if [[ -n "$IDENTITY" ]]; then
  SIGN=(codesign --force --timestamp --options runtime --sign "$IDENTITY")
else
  SIGN=(codesign --force --sign -)
fi

is_macho() { file -b "$1" | grep -q 'Mach-O'; }

sign_bundle() {
  local bundle="$1" main nested f
  [[ -d "$bundle/Contents" ]] || die "$bundle is not a bundle"
  main="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$bundle/Contents/Info.plist")"

  # Nested bundles first (depth-first).
  if [[ -d "$bundle/Contents/PlugIns" ]]; then
    for nested in "$bundle"/Contents/PlugIns/*; do
      [[ -d "$nested/Contents" ]] && sign_bundle "$nested"
    done
  fi
  # Then loose code: libraries before executables, since an executable's
  # signature doesn't cover them but its loads must find them signed.
  for pass in dylib exe; do
    while IFS= read -r -d '' f; do
      [[ "$(basename "$f")" == "$main" && "$(dirname "$f")" == "$bundle/Contents/MacOS" ]] && continue
      is_macho "$f" || continue
      case "$pass:$(file -b "$f")" in
        dylib:*dynamically\ linked\ shared\ library*|dylib:*bundle*) "${SIGN[@]}" "$f" ;;
        exe:*executable*) "${SIGN[@]}" "$f" ;;
      esac
    done < <(find "$bundle/Contents" -type f \( -path '*/Contents/MacOS/*' -o -path '*/Contents/Frameworks/*' -o -path '*/Contents/Helpers/*' \) \
               -not -path "$bundle/Contents/PlugIns/*" -print0)
  done
  "${SIGN[@]}" "$bundle"
  codesign --verify --strict --deep "$bundle"
}

sign_loose() {
  local dir="$1" f pass
  for pass in dylib exe; do
    while IFS= read -r -d '' f; do
      is_macho "$f" || continue
      case "$pass:$(file -b "$f")" in
        dylib:*dynamically\ linked\ shared\ library*) "${SIGN[@]}" "$f" ;;
        exe:*executable*) "${SIGN[@]}" "$f" ;;
      esac
    done < <(find "$dir" -type f -print0)
  done
}

(( $# )) || die "usage: scripts/sign.sh <bundle or folder>..."
if [[ -z "$IDENTITY" ]]; then
  echo "Signing ad hoc (set DEVELOPER_ID_APP to sign for distribution)" >&2
fi
for b in "$@"; do
  if [[ -d "${b%/}/Contents" ]]; then sign_bundle "${b%/}"; else sign_loose "${b%/}"; fi
done
