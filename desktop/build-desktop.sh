#!/bin/bash
# Build the Amux macOS desktop app
# Requires: Xcode or Command Line Tools with matching Swift SDK
set -euo pipefail

# Run from the repo root so Amux.app builds there, regardless of where invoked.
cd "$(dirname "$0")/.."

APP="Amux.app"
SRC="desktop/amux-desktop.swift"
BIN="$APP/Contents/MacOS/amux"
ICON_SRC="assets/icon-512.png"
ICNS="$APP/Contents/Resources/AppIcon.icns"

echo "Building $APP from $SRC..."

# Create app bundle structure
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

# Compile
if xcrun swiftc -O -o "$BIN" "$SRC" -framework Cocoa -framework WebKit 2>/dev/null; then
    echo "  Compiled successfully"
else
    echo "  !! Swift compilation failed."
    echo "  Common fix: xcode-select --install  (or reinstall Command Line Tools)"
    echo "  The existing binary in $BIN may still work."
    exit 1
fi

# Generate .icns from icon-512.png if available
if [ -f "$ICON_SRC" ] && command -v sips &>/dev/null && command -v iconutil &>/dev/null; then
    ICONSET=$(mktemp -d)/AppIcon.iconset
    mkdir -p "$ICONSET"
    for sz in 16 32 64 128 256 512; do
        sips -z $sz $sz "$ICON_SRC" --out "$ICONSET/icon_${sz}x${sz}.png" &>/dev/null
        dbl=$((sz * 2))
        if [ $dbl -le 1024 ]; then
            sips -z $dbl $dbl "$ICON_SRC" --out "$ICONSET/icon_${sz}x${sz}@2x.png" &>/dev/null
        fi
    done
    iconutil -c icns -o "$ICNS" "$ICONSET" 2>/dev/null && echo "  Icon generated" || true
    rm -rf "$(dirname "$ICONSET")"
fi

# Write Info.plist
cat > "$APP/Contents/Info.plist" << 'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>Amux</string>
    <key>CFBundleDisplayName</key>
    <string>Amux</string>
    <key>CFBundleIdentifier</key>
    <string>com.amux.desktop</string>
    <key>CFBundleVersion</key>
    <string>1.0</string>
    <key>CFBundleShortVersionString</key>
    <string>1.0</string>
    <key>CFBundleExecutable</key>
    <string>amux</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
    <key>NSAppTransportSecurity</key>
    <dict>
        <key>NSAllowsLocalNetworking</key>
        <true/>
    </dict>
</dict>
</plist>
PLIST

echo "Done — open $APP or: open Amux.app"
