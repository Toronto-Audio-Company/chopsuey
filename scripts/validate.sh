#!/usr/bin/env bash
# Builds the bundles and runs every validator against them:
#
#   pluginval (strictness 10, every sample rate x block size below) on the
#     VST3 and the AU, release build and rt-checks debug build
#   Steinberg's VST3 validator (extensive mode) on the VST3
#   clap-validator on the CLAP, release and rt-checks builds
#   auval -strict on the AU
#
#   scripts/validate.sh [--no-build] [--universal] [--skip-gui]
#
#   --no-build   validate what's already in target/bundled
#   --universal  build arm64 + x86_64 bundles first
#   --skip-gui   skip pluginval's editor tests (headless machines)
#
# The rt-checks build is a debug build with the `rt-checks` feature, which
# panics on any allocation in process() (nih_plug's assert_process_allocs).
# The stem model is left out: validators don't need it.
#
# Logs go to target/validation/. Runs everything, prints a summary, and
# exits non-zero if anything failed. The AU is validated by copying it to
# ~/Library/Audio/Plug-Ins/Components for the duration (auval only sees
# installed components); a copy already there is put back afterwards.
source "$(dirname "$0")/common.sh"
source "$ROOT/scripts/tools.sh"

BUILD=1 BUNDLE_ARGS=(--no-model) SKIP_GUI=()
for arg in "$@"; do
  case "$arg" in
    --no-build) BUILD=0 ;;
    --universal) BUNDLE_ARGS+=(--universal) ;;
    --skip-gui) SKIP_GUI=(--skip-gui-tests) ;;
    *) die "unknown option $arg" ;;
  esac
done

SAMPLE_RATES=44100,48000,88200,96000,192000
BLOCK_SIZES=32,64,128,256,512,1024,2048
LOGS="$TARGET_DIR/validation"
RT="$BUNDLED/rt-checks"
HAVE_CMAKE=0; command -v cmake >/dev/null && HAVE_CMAKE=1

if (( BUILD )); then
  scripts/bundle.sh "${BUNDLE_ARGS[@]}"
  if (( HAVE_CMAKE )); then packaging/au/build.sh; else warn "cmake not found (brew install cmake): skipping the AU"; fi
  scripts/bundle.sh --rt-checks
fi
step "Fetching validators"
fetch_tools # from tools.sh: pluginval, clap-validator, the VST3 validator

rm -rf "$LOGS" && mkdir -p "$LOGS"
RESULTS=()
FAILED=0
# check <name> <log> <command...>: runs it, records PASS/FAIL.
check() {
  local name="$1" log="$LOGS/$2.log"; shift 2
  printf '%-44s' "$name"
  if "$@" >"$log" 2>&1; then
    echo PASS; RESULTS+=("PASS  $name")
  else
    echo "FAIL  (see ${log#"$ROOT"/})"; RESULTS+=("FAIL  $name  ->  ${log#"$ROOT"/}"); FAILED=1
  fi
}
pluginval() {
  "$PLUGINVAL" --strictness-level 10 --sample-rates "$SAMPLE_RATES" --block-sizes "$BLOCK_SIZES" \
    --timeout-ms 600000 ${SKIP_GUI[@]+"${SKIP_GUI[@]}"} --validate "$1"
}
# (Its output is coloured whatever the terminal: strip that for the log.)
clap_validator() {
  set -o pipefail
  "$CLAPVAL" validate --only-failed "$1" 2>&1 | sed $'s/\x1b\\[[0-9;]*m//g'
}

step "Validating"
[[ -d "$BUNDLED/$NAME.vst3" ]] || die "nothing to validate: run without --no-build"
check "pluginval VST3"                 pluginval-vst3       pluginval "$BUNDLED/$NAME.vst3"
check "Steinberg VST3 validator (-e)"  vst3-validator       "$VST3VAL" -e "$BUNDLED/$NAME.vst3"
check "clap-validator"                 clap-validator       clap_validator "$BUNDLED/$NAME.clap"
if [[ -d "$RT/$NAME.vst3" ]]; then
  check "pluginval VST3 (rt-checks debug)" pluginval-vst3-rt  pluginval "$RT/$NAME.vst3"
  check "clap-validator (rt-checks debug)" clap-validator-rt  clap_validator "$RT/$NAME.clap"
else
  warn "no rt-checks build in $RT: skipped (run without --no-build)"
fi

if [[ -d "$BUNDLED/$NAME.component" ]]; then
  COMPONENTS="$HOME/Library/Audio/Plug-Ins/Components"
  INSTALLED="$COMPONENTS/$NAME.component"
  BACKUP="$LOGS/previously-installed.component"
  mkdir -p "$COMPONENTS"
  [[ -e "$INSTALLED" ]] && mv "$INSTALLED" "$BACKUP"
  restore_component() {
    rm -rf "$INSTALLED"
    if [[ -e "$BACKUP" ]]; then mv "$BACKUP" "$INSTALLED"; fi
    killall -9 AudioComponentRegistrar 2>/dev/null || true
  }
  trap restore_component EXIT
  cp -R "$BUNDLED/$NAME.component" "$INSTALLED"
  # Make the AU registry notice it now rather than eventually.
  killall -9 AudioComponentRegistrar 2>/dev/null || true
  check "auval -strict $AU_TYPE $AU_SUBTYPE $AU_MANUFACTURER" auval \
    auval -strict -v "$AU_TYPE" "$AU_SUBTYPE" "$AU_MANUFACTURER"
  check "pluginval AU"                 pluginval-au         pluginval "$INSTALLED"
else
  warn "no AU in $BUNDLED: auval skipped"
fi

step "Summary"
printf '%s\n' "${RESULTS[@]}"
exit "$FAILED"
