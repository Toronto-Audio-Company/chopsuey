# Shared settings for the build, validation and release scripts.
# Sourced, not run: `source "$(dirname "$0")/common.sh"`.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

NAME=chopsuey                                 # bundle names (bundler.toml)
PACKAGE=tempo_sampler                         # the plugin crate
HELPER_CRATE=stems_helper                     # the stem separation helper (if it exists)
HELPER_BIN=chopsuey-stems
VENDOR="Toronto Audio Company"
ID_PREFIX=com.torontoaudiocompany
PKG_ID="$ID_PREFIX.chopsuey"
AU_TYPE=aumu AU_SUBTYPE=Chsy AU_MANUFACTURER=TAud
MIN_MACOS=11.0

VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BUNDLED="$TARGET_DIR/bundled"
TOOLS="${CHOPSUEY_TOOLS:-$TARGET_DIR/tools}"     # validators (scripts/tools.sh)
MODEL="$ROOT/models/htdemucs_fp16weights.onnx"
MODEL_URL=https://huggingface.co/StemSplitio/htdemucs-onnx/resolve/main/htdemucs_fp16weights.onnx

export MACOSX_DEPLOYMENT_TARGET="$MIN_MACOS"

step() { printf '\n== %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

has_helper() { [[ -f "$ROOT/$HELPER_CRATE/Cargo.toml" ]]; }

# The newest macOS any Mach-O file under the given paths needs (every
# architecture), e.g. 13.4. Never lower than MIN_MACOS.
bundle_min_macos() {
  find "$@" -type f -perm -u+x -print0 |
    xargs -0 -n1 sh -c 'file -b "$0" | grep -q Mach-O && otool -l "$0"' 2>/dev/null |
    awk -v min="$MIN_MACOS" '/LC_BUILD_VERSION/ { v = 1 } v && $1 == "minos" { print $2; v = 0 } END { print min }' |
    sort -t. -k1,1n -k2,2n -k3,3n | tail -1
}

# Downloads the stem model once (it isn't in git: 166 MB). It goes to a
# temporary name first so an interrupted download is never mistaken for the
# real file.
fetch_model() {
  [[ -f "$MODEL" ]] && return
  [[ -z "${CI:-}" ]] || die "CI must not download the stem model (use --no-model)"
  step "Downloading the stem model (166 MB, first time only)"
  mkdir -p "$(dirname "$MODEL")"
  curl -fL --progress-bar -o "$MODEL.part" "$MODEL_URL"
  if (( $(stat -f%z "$MODEL.part") < 100000000 )); then
    die "the download looks wrong (too small). Delete $MODEL.part and try again."
  fi
  mv "$MODEL.part" "$MODEL"
}
