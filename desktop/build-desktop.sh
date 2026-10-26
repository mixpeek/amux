#!/bin/bash
# Build the Amux macOS desktop app
# Requires: Xcode or Command Line Tools with matching Swift SDK
set -euo pipefail

# Run from the repo root so Amux.app builds there, regardless of where invoked.
cd "$(dirname "$0")/.."

APP="Amux.app"
# One binary: the app, the pinned-note overlay and the menu bar readout. The
# last two keep their own standalone builds; -D AMUX_APP drops their entry
# points here.
SRC="desktop/amux-desktop.swift desktop/amux-settings.swift desktop/amux-update.swift desktop/amux-workspace.swift desktop/amux-snapzones.swift desktop/amux-panegrid.swift desktop/amux-panes.swift scripts/pinned-overlay.swift scripts/amux-menubar.swift"
BIN="$APP/Contents/MacOS/amux"
ICON_SRC="assets/icon-512.png"
ICNS="$APP/Contents/Resources/AppIcon.icns"

echo "Building $APP from $SRC..."

# Create app bundle structure
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

# Compile. AMUX_DESKTOP_UNIVERSAL=1 builds arm64 + x86_64 and joins them, so
# one download runs on Apple silicon and Intel Macs (CI does this).
# The oldest macOS the app runs on. Every build names it: without -target,
# swiftc stamps the BUILD machine's SDK as the minimum (macOS 26 on a current
# Xcode), and an older Mac refuses the app with "You can't use this version of
# the application with this version of macOS" (2026-10-26, a user report)
# while Info.plist still claimed 12.0.
MIN_MACOS=12.0
FRAMEWORKS="-framework Cocoa -framework WebKit -framework Carbon -framework UserNotifications -framework ServiceManagement"
compile() {  # $1 = output, $2 = optional -target
    # shellcheck disable=SC2086  # SRC and FRAMEWORKS are word lists
    xcrun swiftc -O -parse-as-library -D AMUX_APP ${2:+-target "$2"} -o "$1" $SRC $FRAMEWORKS
}
if [ "${AMUX_DESKTOP_UNIVERSAL:-0}" = 1 ]; then
    TMPB=$(mktemp -d)
    compile "$TMPB/arm64" "arm64-apple-macos$MIN_MACOS" && compile "$TMPB/x86_64" "x86_64-apple-macos$MIN_MACOS" \
        && lipo -create -output "$BIN" "$TMPB/arm64" "$TMPB/x86_64" && OK=1 || OK=0
    rm -rf "$TMPB"
else
    compile "$BIN" "$(uname -m)-apple-macos$MIN_MACOS" && OK=1 || OK=0
fi
if [ "$OK" = 1 ]; then
    # Check what the binary really requires, per architecture, not what we asked for.
    for MINOS in $(otool -l "$BIN" | awk '/LC_BUILD_VERSION/{f=1} f&&/minos/{print $2; f=0}'); do
        if [ "$(printf '%s\n%s\n' "$MINOS" "$MIN_MACOS" | sort -V | tail -1)" != "$MIN_MACOS" ]; then
            echo "  !! verdict=desktop_min_os_mismatch binary needs macOS $MINOS but Info.plist promises $MIN_MACOS"
            exit 1
        fi
    done
    echo "  Compiled successfully ($(lipo -archs "$BIN" 2>/dev/null), runs on macOS $MIN_MACOS and later)"
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
    <string>2.0</string>
    <key>CFBundleShortVersionString</key>
    <string>2.0</string>
    <key>CFBundleExecutable</key>
    <string>amux</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSMicrophoneUsageDescription</key>
    <string>amux uses the microphone for voice input and recordings you start.</string>
    <key>NSCameraUsageDescription</key>
    <string>amux uses the camera for recordings you start.</string>
    <key>CFBundleURLTypes</key>
    <array>
        <dict>
            <key>CFBundleURLName</key>
            <string>io.amux.link</string>
            <key>CFBundleURLSchemes</key>
            <array><string>amux</string></array>
        </dict>
    </array>
    <key>NSAppTransportSecurity</key>
    <dict>
        <key>NSAllowsLocalNetworking</key>
        <true/>
    </dict>
</dict>
</plist>
PLIST
/usr/libexec/PlistBuddy -c "Set :LSMinimumSystemVersion $MIN_MACOS" "$APP/Contents/Info.plist"   # the one value, from MIN_MACOS

# The commit this build came from, so Settings > Updates can compare it with
# the desktop-latest release. "-dirty" when the app's sources have uncommitted
# changes: that build is no commit's.
COMMIT=${GITHUB_SHA:-$(git rev-parse HEAD 2>/dev/null || true)}
if [ -n "$COMMIT" ] && [ -z "${GITHUB_SHA:-}" ] && [ -n "$(git status --porcelain -- desktop scripts/pinned-overlay.swift scripts/amux-menubar.swift 2>/dev/null)" ]; then
    COMMIT="$COMMIT-dirty"
fi
/usr/libexec/PlistBuddy -c "Add :AmuxBuildCommit string ${COMMIT:-}" -c "Add :AmuxBuildDate string $(date -u '+%Y-%m-%d %H:%M UTC')" "$APP/Contents/Info.plist"
echo "  Built from ${COMMIT:-an unknown commit}"

# Ad-hoc sign so macOS keeps the microphone and notification permissions
# across rebuilds of the same bundle id.
codesign --force --deep --sign - "$APP" >/dev/null 2>&1 && echo "  Signed (ad hoc)" || echo "  !! codesign failed; permissions may be asked again"

# Install to /Applications if requested or by default on build
# AMUX_DESKTOP_NO_INSTALL=1 leaves the bundle in the repo root (CI zips it).
if [ "${AMUX_DESKTOP_NO_INSTALL:-0}" != 1 ] && [ -d /Applications ]; then
    rm -rf "/Applications/$APP"
    cp -R "$APP" "/Applications/$APP"
    echo "  Installed to /Applications/$APP"
fi

if [ "${AMUX_DESKTOP_NO_INSTALL:-0}" = 1 ]; then
    echo "Done: $APP (not installed)"
else
    echo "Done: open /Applications/$APP"
fi
