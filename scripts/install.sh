#!/usr/bin/env bash
# Downloads the stem model (first time only), then tests, builds and
# installs chopsuey as a VST3 for the current user, all in one go.
#
#   scripts/install.sh
#
# Stops without installing if any test fails.
set -euo pipefail
cd "$(dirname "$0")/.."

MODEL=models/htdemucs_fp16weights.onnx
MODEL_URL=https://huggingface.co/StemSplitio/htdemucs-onnx/resolve/main/htdemucs_fp16weights.onnx
BUNDLE="target/bundled/chopsuey.vst3"
DEST="$HOME/Library/Audio/Plug-Ins/VST3"

if ! command -v cargo >/dev/null; then
  echo "Rust isn't installed. Get it from https://rustup.rs, then run this again." >&2
  exit 1
fi

# The stem separation model isn't in git (166 MB): fetch it once. It goes to
# a temporary name first so an interrupted download is never mistaken for
# the real file.
if [[ ! -f "$MODEL" ]]; then
  echo "== Downloading the stem model (166 MB, first time only)"
  mkdir -p models
  curl -fL --progress-bar -o "$MODEL.part" "$MODEL_URL"
  if (( $(stat -f%z "$MODEL.part") < 100000000 )); then
    echo "The download looks wrong (too small). Delete $MODEL.part and try again." >&2
    exit 1
  fi
  mv "$MODEL.part" "$MODEL"
fi

echo "== Testing"
cargo test --release --lib

echo "== Bundling"
cargo xtask bundle tempo_sampler --release

echo "== Adding the stem model and the WebGPU library"
mkdir -p "$BUNDLE/Contents/Resources"
cp "$MODEL" "$BUNDLE/Contents/Resources/htdemucs.onnx"
# Dawn (WebGPU on Metal) is loaded from next to the plugin binary (@rpath = @loader_path).
DAWN=target/release/libwebgpu_dawn.dylib
if [[ ! -e "$DAWN" ]]; then
  echo "Missing $DAWN (built by the ort crate's webgpu feature)." >&2
  exit 1
fi
cp -L "$DAWN" "$BUNDLE/Contents/MacOS/"
# Adding files invalidates the bundle's signature, so sign again (ad hoc):
# the nested library first, then the bundle.
codesign --force --sign - "$BUNDLE/Contents/MacOS/libwebgpu_dawn.dylib"
codesign --force --sign - "$BUNDLE"
codesign --verify --deep --strict "$BUNDLE"

echo "== Installing to $DEST"
mkdir -p "$DEST"
rm -rf "$DEST/chopsuey.vst3"
# The app used to be called "Tempo Sampler": remove that build too, since it
# has the same plugin ID and Live would list two copies.
rm -rf "$DEST/Tempo Sampler.vst3"
cp -R "$BUNDLE" "$DEST/"
echo "Installed. Restart Live to load the new build."
