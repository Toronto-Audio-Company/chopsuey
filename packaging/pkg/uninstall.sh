#!/bin/sh
# Removes chopsuey (VST3, CLAP and Audio Unit) installed by the chopsuey
# installer, and the installer's receipts. Asks for your password.
#
#   sh uninstall-chopsuey.sh
#
# Your exported WAVs (~/Music/chopsuey) and settings are left alone.
set -eu

PLUGINS="/Library/Audio/Plug-Ins"
BUNDLES="$PLUGINS/VST3/chopsuey.vst3 $PLUGINS/CLAP/chopsuey.clap $PLUGINS/Components/chopsuey.component"
RECEIPTS="com.torontoaudiocompany.chopsuey.vst3 com.torontoaudiocompany.chopsuey.clap com.torontoaudiocompany.chopsuey.au"

if [ "$(id -u)" -ne 0 ]; then
  echo "Removing chopsuey needs administrator rights."
  exec sudo /bin/sh "$0" "$@"
fi

found=0
for b in $BUNDLES; do
  if [ -e "$b" ]; then
    rm -rf "$b"
    echo "Removed $b"
    found=1
  fi
done
for r in $RECEIPTS; do
  pkgutil --pkg-info "$r" >/dev/null 2>&1 && pkgutil --forget "$r" >/dev/null && found=1
done
killall -9 AudioComponentRegistrar 2>/dev/null || true

if [ "$found" -eq 0 ]; then
  echo "chopsuey isn't installed in $PLUGINS."
else
  echo "chopsuey is uninstalled. Restart your DAW."
fi
