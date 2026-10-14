#!/usr/bin/env bash
# TackleCast Linux Launcher Script
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# Find binary
if [[ -f "$SCRIPT_DIR/tacklecast" ]]; then
    BIN="$SCRIPT_DIR/tacklecast"
elif [[ -f "$ROOT_DIR/target/release/tacklecast" ]]; then
    BIN="$ROOT_DIR/target/release/tacklecast"
elif [[ -f "$ROOT_DIR/target/debug/tacklecast" ]]; then
    BIN="$ROOT_DIR/target/debug/tacklecast"
else
    echo "TackleCast binary not found. Building release binary..."
    cd "$ROOT_DIR"
    cargo build --release
    BIN="$ROOT_DIR/target/release/tacklecast"
fi

# Check permissions for video/audio devices
if [[ -d "/dev" ]]; then
    VIDEO_DEVS=$(ls /dev/video* 2>/dev/null || true)
    if [[ -n "$VIDEO_DEVS" ]]; then
        for dev in $VIDEO_DEVS; do
            if [[ ! -r "$dev" || ! -w "$dev" ]]; then
                echo "Warning: No read/write access to $dev."
                echo "To fix device permissions, run:"
                echo "  sudo usermod -a -G video,audio $USER"
                echo "and log out then log back in."
                break
            fi
        done
    fi
fi

# Launch
if [[ -d "$SCRIPT_DIR/lib" ]]; then
    export LD_LIBRARY_PATH="$SCRIPT_DIR/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
elif [[ -d "$ROOT_DIR/.deps/ffmpeg/lib" ]]; then
    export LD_LIBRARY_PATH="$ROOT_DIR/.deps/ffmpeg/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
fi
if command -v ldd >/dev/null; then
    MISSING_LIBS=$(ldd "$BIN" | awk '/not found/ { print $1 }')
    if [[ -n "$MISSING_LIBS" ]]; then
        printf 'Required libraries are missing:\n%s\nKeep the bundled lib/ folder next to tacklecast and install your desktop audio/graphics runtime.\n' "$MISSING_LIBS" >&2
        exit 1
    fi
fi
echo "Starting TackleCast from $BIN..."
exec "$BIN" "$@"
