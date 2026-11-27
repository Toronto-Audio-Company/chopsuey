#!/usr/bin/env bash
# Downloads the stem model (first time only), then tests, builds and
# installs chopsuey as a VST3 for the current user, all in one go.
#
#   scripts/install.sh
#
# Stops without installing if any test fails. This is the developer's
# install (ad hoc signed, ~/Library); scripts/release.sh makes the installer.
source "$(dirname "$0")/common.sh"

DEST="$HOME/Library/Audio/Plug-Ins/VST3"

if ! command -v cargo >/dev/null; then
  die "Rust isn't installed. Get it from https://rustup.rs, then run this again."
fi

fetch_model

step "Testing"
# The stems test runs the real helper from target/release, and `cargo test`
# doesn't build another crate's binaries, so a fresh clone needs it built first.
cargo build --release -p "$HELPER_CRATE"
cargo test --release --workspace

# Builds, adds the stem helper, Dawn and the model, and signs ad hoc.
scripts/bundle.sh
BUNDLE="$BUNDLED/$NAME.vst3"

step "Installing to $DEST"
mkdir -p "$DEST"
rm -rf "$DEST/$NAME.vst3"
# The app used to be called "Tempo Sampler": remove that build too, since it
# has the same plugin ID and Live would list two copies.
rm -rf "$DEST/Tempo Sampler.vst3"
cp -R "$BUNDLE" "$DEST/"

# The takeout box as the Finder icon, on the installed plugin and on the
# folder dragged WAVs go to. Finder ignores icons inside .vst3 bundles, so
# this is a custom icon like pasting one in Get Info. It's added after
# signing, to the installed copy only: the plugin's code signature still
# verifies (Live only checks that), only --strict objects to Finder data.
step "Setting Finder icons"
EXPORTS="$HOME/Music/chopsuey"
mkdir -p "$EXPORTS"
for target in "$DEST/$NAME.vst3" "$EXPORTS"; do
  swift scripts/icon.swift set assets/chopsuey.icns "$target" 2>/dev/null \
    || echo "(Couldn't set the icon on $target; the plugin works without it.)"
done
codesign --verify "$DEST/$NAME.vst3"
echo "Installed. Restart Live to load the new build."
