#!/usr/bin/env bash
# Rebuilds assets/chopsuey.icns (the Finder icon for the plugin and the
# exports folder) from assets/logo.svg: the logo on a white macOS-style
# tile. Run after editing the logo; needs rsvg-convert (brew install librsvg).
#
#   scripts/make_icon.sh
set -euo pipefail
cd "$(dirname "$0")/.."
command -v rsvg-convert >/dev/null || { echo "Needs rsvg-convert: brew install librsvg" >&2; exit 1; }

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# The logo's shapes, placed in the middle of a 1024 canvas on Apple's icon
# grid: an 824 tile (radius 185) with a soft shadow, logo 600 across.
PATHS=$(grep '^ *<path ' assets/logo.svg)
cat > "$WORK/icon.svg" <<SVG
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024">
  <defs>
    <linearGradient id="tile" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#ffffff"/>
      <stop offset="1" stop-color="#d9d9d9"/>
    </linearGradient>
    <filter id="shadow" x="-10%" y="-10%" width="120%" height="130%">
      <feDropShadow dx="0" dy="12" stdDeviation="14" flood-color="#000000" flood-opacity="0.3"/>
    </filter>
  </defs>
  <rect x="100" y="100" width="824" height="824" rx="185" fill="url(#tile)" filter="url(#shadow)"/>
  <svg x="212" y="212" width="600" height="600" viewBox="0 0 24 24" fill="none" stroke="#0b0b0b"
       stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
$PATHS
  </svg>
</svg>
SVG

SET="$WORK/chopsuey.iconset"
mkdir "$SET"
for size in 16 32 128 256 512; do
  rsvg-convert -w $size -h $size "$WORK/icon.svg" -o "$SET/icon_${size}x${size}.png"
  rsvg-convert -w $((size * 2)) -h $((size * 2)) "$WORK/icon.svg" -o "$SET/icon_${size}x${size}@2x.png"
done
iconutil -c icns "$SET" -o assets/chopsuey.icns
cp "$SET/icon_512x512@2x.png" assets/icon.png
echo "assets/chopsuey.icns"
