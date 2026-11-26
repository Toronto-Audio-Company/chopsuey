#!/usr/bin/env bash
# Fetches the plugin validators into target/tools (gitignored), once.
#
#   scripts/tools.sh            # all of them
#   scripts/tools.sh pluginval clap-validator vst3-validator
#
# Prints nothing else on success. Pinned versions and checksums are below;
# bump them together. CI caches target/tools keyed on this file.
source "$(dirname "$0")/common.sh"

PLUGINVAL_URL=https://github.com/Tracktion/pluginval/releases/download/v1.0.4/pluginval_macOS.zip
PLUGINVAL_SHA=3c4c533bda0c5059eea3ddaea752d757ee2025041f0f47e6bcb0e87f6082b29f
CLAPVAL_URL=https://github.com/free-audio/clap-validator/releases/download/0.4.1/clap-validator-0.4.1-127-g152b982-macos-universal.zip
CLAPVAL_SHA=bbec8cd7d18274e549d5d8c12ece3cec54be966129388dd2e742b9957f2ba9f1
VST3SDK_TAG=v3.8.0_build_66

PLUGINVAL="$TOOLS/pluginval/pluginval.app/Contents/MacOS/pluginval"
CLAPVAL="$TOOLS/clap-validator/clap-validator"
VST3VAL="$TOOLS/vst3sdk-$VST3SDK_TAG/build/bin/Release/validator"

# download <url> <sha256> <file>
download() {
  curl -fsSL --retry 3 -o "$3.part" "$1"
  local got
  got="$(shasum -a 256 "$3.part" | cut -d' ' -f1)"
  [[ "$got" == "$2" ]] || die "checksum mismatch for $1 (got $got)"
  mv "$3.part" "$3"
}

fetch_pluginval() {
  [[ -x "$PLUGINVAL" ]] && return
  echo "Fetching pluginval" >&2
  rm -rf "$TOOLS/pluginval" && mkdir -p "$TOOLS/pluginval"
  download "$PLUGINVAL_URL" "$PLUGINVAL_SHA" "$TOOLS/pluginval/pluginval.zip"
  unzip -q -o "$TOOLS/pluginval/pluginval.zip" -d "$TOOLS/pluginval"
}

fetch_clap_validator() {
  [[ -x "$CLAPVAL" ]] && return
  echo "Fetching clap-validator" >&2
  rm -rf "$TOOLS/clap-validator" && mkdir -p "$TOOLS/clap-validator"
  download "$CLAPVAL_URL" "$CLAPVAL_SHA" "$TOOLS/clap-validator/clap-validator.zip"
  # A zip holding a tarball holding the binary.
  unzip -q -o "$TOOLS/clap-validator/clap-validator.zip" -d "$TOOLS/clap-validator"
  tar -xzf "$TOOLS"/clap-validator/clap-validator-*.tar.gz -C "$TOOLS/clap-validator"
  local bin
  bin="$(find "$TOOLS/clap-validator" -type f -name clap-validator -perm -u+x | head -1)"
  [[ -n "$bin" ]] || die "no clap-validator binary in the download"
  [[ "$bin" == "$CLAPVAL" ]] || mv "$bin" "$CLAPVAL"
}

# Steinberg doesn't publish validator binaries, so build it from the SDK
# (only the parts it needs: no VSTGUI, no examples). Takes a minute or two.
fetch_vst3_validator() {
  [[ -x "$VST3VAL" ]] && return
  command -v cmake >/dev/null || die "cmake is needed to build Steinberg's VST3 validator (brew install cmake)"
  echo "Building Steinberg's VST3 validator ($VST3SDK_TAG)" >&2
  local sdk="$TOOLS/vst3sdk-$VST3SDK_TAG"
  if [[ ! -f "$sdk/pluginterfaces/base/funknown.h" ]]; then
    rm -rf "$sdk"
    git clone -q --depth 1 --branch "$VST3SDK_TAG" https://github.com/steinbergmedia/vst3sdk.git "$sdk"
    git -C "$sdk" -c advice.detachedHead=false submodule update -q --init --depth 1 \
      base pluginterfaces public.sdk cmake
  fi
  cmake -S "$sdk" -B "$sdk/build" -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_POLICY_VERSION_MINIMUM=3.5 \
    -DSMTG_ENABLE_VSTGUI_SUPPORT=OFF -DSMTG_ENABLE_VST3_PLUGIN_EXAMPLES=OFF \
    -DSMTG_ENABLE_VST3_HOSTING_EXAMPLES=OFF -DSMTG_RUN_VST_VALIDATOR=OFF \
    -DSMTG_CREATE_PLUGIN_LINK=OFF -DSMTG_ENABLE_TARGET_VARS_LOG=OFF >/dev/null
  cmake --build "$sdk/build" --config Release --target validator -j "$(sysctl -n hw.ncpu)" >/dev/null
  # Single-config generators put it in bin/Release too, but be sure.
  [[ -x "$VST3VAL" ]] || {
    local bin; bin="$(find "$sdk/build/bin" -type f -name validator -perm -u+x | head -1)"
    [[ -n "$bin" ]] || die "the VST3 validator didn't build"
    mkdir -p "$(dirname "$VST3VAL")" && cp "$bin" "$VST3VAL"
  }
}

fetch_tools() {
  mkdir -p "$TOOLS"
  local want=("$@")
  (( ${#want[@]} )) || want=(pluginval clap-validator vst3-validator)
  for t in "${want[@]}"; do
    case "$t" in
      pluginval) fetch_pluginval ;;
      clap-validator) fetch_clap_validator ;;
      vst3-validator) fetch_vst3_validator ;;
      *) die "unknown tool '$t'" ;;
    esac
  done
}

# Other scripts source this file for the paths above; only fetch when run.
if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then fetch_tools "$@"; fi
