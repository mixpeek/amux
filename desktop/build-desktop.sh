#!/bin/bash
# Build the Amux macOS desktop app
# Requires: Xcode or Command Line Tools with matching Swift SDK
set -euo pipefail

# Run from the repo root so Amux.app builds there, regardless of where invoked.
cd "$(dirname "$0")/.."

APP="Amux.app"
# An agent's build is a separate app, "Amux Dev" (com.amux.desktop.dev), so a
# test copy never shares the owner's preferences, Dock icon, notifications or
# amux:// links (owner, 2026-11-09: "maybe you create a new amux app for
# development that way my current amux macos app experience isnt
# intervened"). AMUX_DESKTOP_DEV=0|1 picks either way; installing builds the
# real one. Its Microphone and Screen Recording grants are its own.
DEV=0
if [ "${AMUX_DESKTOP_INSTALL:-0}" != 1 ] && { [ -n "${CLAUDECODE:-}" ] || [ -n "${AMUX_SESSION:-}" ]; }; then DEV=1; fi
case "${AMUX_DESKTOP_DEV:-}" in 1) DEV=1 ;; 0) DEV=0 ;; esac
BUNDLE_ID=com.amux.desktop
if [ "$DEV" = 1 ]; then BUNDLE_ID=com.amux.desktop.dev; fi
# One binary: the app, the pinned-note overlay and the menu bar readout. The
# last two keep their own standalone builds; -D AMUX_APP drops their entry
# points here.
SRC="desktop/amux-desktop.swift desktop/amux-settings.swift desktop/amux-storage.swift desktop/amux-update.swift desktop/amux-workspace.swift desktop/amux-find.swift desktop/amux-dictation-core.swift desktop/amux-dictation.swift desktop/amux-rehome.swift desktop/amux-snapzones.swift desktop/amux-panegrid.swift desktop/amux-panes.swift desktop/amux-tabcolors.swift desktop/amux-recorder-core.swift desktop/amux-recorder.swift desktop/amux-statusmenu.swift desktop/amux-computer.swift desktop/amux-agentview.swift scripts/pinned-overlay.swift scripts/amux-menubar.swift"
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
# ScreenCaptureKit (system audio for the recorder) exists from macOS 13, so it
# is weak-linked: on 12 the app runs and records the microphone only.
FRAMEWORKS="-framework Cocoa -framework WebKit -framework Carbon -framework UserNotifications -framework ServiceManagement -framework AVFoundation -framework CoreMedia -framework PDFKit -framework Speech -Xlinker -weak_framework -Xlinker ScreenCaptureKit"
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
if [ "$DEV" = 1 ]; then
    # Its own id and name, and no amux:// handler: a link the owner clicks opens their app.
    /usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier $BUNDLE_ID" -c "Set :CFBundleName Amux Dev" \
        -c "Set :CFBundleDisplayName Amux Dev" -c "Delete :CFBundleURLTypes" "$APP/Contents/Info.plist"
    echo "  Development build: Amux Dev ($BUNDLE_ID), separate from the installed Amux"
fi

# The commit this build came from, so Settings > Updates can compare it with
# the desktop-latest release. "-dirty" when the app's sources have uncommitted
# changes: that build is no commit's.
COMMIT=${GITHUB_SHA:-$(git rev-parse HEAD 2>/dev/null || true)}
if [ -n "$COMMIT" ] && [ -z "${GITHUB_SHA:-}" ] && [ -n "$(git status --porcelain -- desktop scripts/pinned-overlay.swift scripts/amux-menubar.swift 2>/dev/null)" ]; then
    COMMIT="$COMMIT-dirty"
fi
/usr/libexec/PlistBuddy -c "Add :AmuxBuildCommit string ${COMMIT:-}" -c "Add :AmuxBuildDate string $(date -u '+%Y-%m-%d %H:%M UTC')" "$APP/Contents/Info.plist"
echo "  Built from ${COMMIT:-an unknown commit}"

# Computer use on this Mac (amux-computer.swift): trycua's cua-driver (MIT),
# pinned to one release and checked against its published SHA-256 before it
# goes in the bundle. The app runs it embedded, so it uses Amux's own
# Accessibility and Screen Recording grants. A failed download builds an app
# without it, and Settings > Computer use then says the driver is missing.
CUA_VER=0.58.1
CUA_SHA=99b06af80422ba77be91932ce6c58fed4934a9565cb74bb5cde1fa0396ed1eb2
CUA_TGZ="cua-driver-rs-$CUA_VER-darwin-universal-binary.tar.gz"
CUA_CACHE="${AMUX_CACHE_DIR:-$HOME/.amux/cache}/cua-driver-$CUA_VER"
if [ ! -x "$CUA_CACHE/cua-driver" ]; then
    CUA_TMP=$(mktemp -d)
    if curl -fsSL -o "$CUA_TMP/$CUA_TGZ" "https://github.com/trycua/cua/releases/download/cua-driver-rs-v$CUA_VER/$CUA_TGZ" \
        && echo "$CUA_SHA  $CUA_TMP/$CUA_TGZ" | shasum -a 256 -c - >/dev/null \
        && mkdir -p "$CUA_TMP/x" && tar -xzf "$CUA_TMP/$CUA_TGZ" -C "$CUA_TMP/x" cua-driver; then
        mkdir -p "$CUA_CACHE" && mv "$CUA_TMP/x/cua-driver" "$CUA_CACHE/cua-driver"
    else
        echo "  !! cua-driver $CUA_VER did not download or failed its checksum: this build has no computer use"
    fi
    rm -rf "$CUA_TMP"
fi
if [ -x "$CUA_CACHE/cua-driver" ]; then
    mkdir -p "$APP/Contents/Helpers"
    cp "$CUA_CACHE/cua-driver" "$APP/Contents/Helpers/cua-driver"
    { printf 'cua-driver %s, from https://github.com/trycua/cua\n\n' "$CUA_VER"; cat desktop/third-party/cua-driver-LICENSE.txt; } \
        > "$APP/Contents/Resources/cua-driver-LICENSE.txt"
    echo "  Bundled cua-driver $CUA_VER (computer use on this Mac)"
fi

# Ad-hoc sign so macOS keeps the microphone and notification permissions
# across rebuilds of the same bundle id.
# Ad hoc, with the bundle id as the designated requirement. macOS keys the
# microphone and Screen Recording grants on the requirement it saw, and an ad
# hoc signature's default one is the binary's hash: every rebuild silently
# lost both grants and asked again (2026-10-28). The id stays the same build
# to build, so a grant given once carries over.
# Helpers are signed first, each under its own identifier. `--deep` on the
# app gave the bundled cua-driver the APP's requirement (identifier
# $BUNDLE_ID), which the driver's own identifier cannot meet, so
# `codesign --verify --deep` failed with "nested code is modified or invalid"
# (desktop CI red from 694a5708c, 2026-11-22).
for h in "$APP"/Contents/Helpers/*; do
    [ -f "$h" ] && { codesign --force --sign - "$h" >/dev/null 2>&1 || echo "  !! codesign failed on $h"; }
done
codesign --force --sign - -r="designated => identifier \"$BUNDLE_ID\"" "$APP" >/dev/null 2>&1 \
  && echo "  Signed (ad hoc, requirement: identifier $BUNDLE_ID)" || echo "  !! codesign failed; permissions may be asked again"
codesign --verify --deep --strict "$APP" 2>/dev/null || echo "  !! codesign --verify --deep failed on $APP: $(codesign --verify --deep --strict "$APP" 2>&1 | tail -1)"

# Install to /Applications if requested or by default on build
# AMUX_DESKTOP_NO_INSTALL=1 leaves the bundle in the repo root (CI zips it).
# An agent's build (Claude Code, an amux worker) leaves it there too unless
# AMUX_DESKTOP_INSTALL=1: a worker's test build of a dirty worktree replaced
# the owner's installed app, Intel-only, until it was restored (2026-11-08).
if [ "${AMUX_DESKTOP_INSTALL:-0}" != 1 ] && { [ -n "${CLAUDECODE:-}" ] || [ -n "${AMUX_SESSION:-}" ]; }; then
    echo "  Not installed: an agent's build stays in $APP (AMUX_DESKTOP_INSTALL=1 installs it)"
    AMUX_DESKTOP_NO_INSTALL=1
fi
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
