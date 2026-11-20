#!/usr/bin/env bash
# Bundles the Pulse tray binary into a minimal macOS app: no Xcode project,
# just the binary, an Info.plist and the icon. LSUIElement keeps it out of
# the Dock, so it lives in the menu bar only.
#
#   scripts/build-macos-app.sh <pulse-binary> <Pulse.app-output-path>
set -eu

BIN="${1:?pass the pulse binary}"
OUT="${2:?pass the .app path}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ICON="$ROOT/crates/pulse/assets/tray.png"

rm -rf "$OUT"
mkdir -p "$OUT/Contents/MacOS" "$OUT/Contents/Resources"
cp "$BIN" "$OUT/Contents/MacOS/pulse"
cp "$ICON" "$OUT/Contents/Resources/icon.png"
# the icon, the way macOS likes it: an iconset of every standard size
ICONSET="$OUT/Contents/Resources/icon.iconset"
mkdir -p "$ICONSET"
for size in 16 32 64 128 256 512; do
  sips -z "$size" "$size" "$ICON" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z "$double" "$double" "$ICON" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$OUT/Contents/Resources/icon.icns" 2>/dev/null \
  || echo "warning: could not build the .icns; the menu bar icon is unaffected"

cat > "$OUT/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleExecutable</key>
	<string>pulse</string>
	<key>CFBundleIconFile</key>
	<string>icon.icns</string>
	<key>CFBundleIdentifier</key>
	<string>club.hack.pulse</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>Pulse</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>0.1.0</string>
	<key>CFBundleVersion</key>
	<string>1</string>
	<key>LSMinimumSystemVersion</key>
	<string>11.0</string>
	<key>LSUIElement</key>
	<true/>
	<key>NSHighResolutionCapable</key>
	<true/>
</dict>
</plist>
PLIST

echo "built $OUT"
