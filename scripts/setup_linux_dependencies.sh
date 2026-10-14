#!/usr/bin/env bash
# Build only the shared FFmpeg components used by a capture-card viewer.
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEPS_DIR="$ROOT_DIR/.deps"
FFMPEG_VERSION=8.1.3
FFMPEG_SHA256=7138d28c96d9d3e3af4ee3d8cad72741f8ffb40da90c1112235dea3ecd3178a3
PREFIX="$DEPS_DIR/ffmpeg"
CONFIG_ID="$FFMPEG_SHA256-minimal-v2"
mkdir -p "$DEPS_DIR"
if [[ ! -f "$PREFIX/build-id" ]] || [[ "$(cat "$PREFIX/build-id")" != "$CONFIG_ID" ]]; then
    ARCHIVE="$DEPS_DIR/ffmpeg-$FFMPEG_VERSION.tar.xz"
    if [[ ! -f "$ARCHIVE" ]]; then
        curl --fail --location --retry 3 "https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VERSION.tar.xz" -o "$ARCHIVE"
    fi
    echo "$FFMPEG_SHA256  $ARCHIVE" | sha256sum --check --status
    tar -xf "$ARCHIVE" -C "$DEPS_DIR"
    pushd "$DEPS_DIR/ffmpeg-$FFMPEG_VERSION" >/dev/null
    ./configure --prefix="$PREFIX" --disable-everything --disable-autodetect \
        --disable-network --disable-programs --disable-doc --disable-static \
        --enable-shared --enable-pic \
        --enable-avcodec --enable-avformat --enable-avdevice --enable-avfilter \
        --enable-swresample --enable-swscale --enable-indev=v4l2 \
        --enable-decoder=mjpeg,rawvideo,h264 --enable-parser=mjpeg,h264
    make -j "${BUILD_JOBS:-2}"
    make install
    cp COPYING.LGPLv2.1 "$PREFIX/LICENSE.txt"
    printf '%s\n' "$CONFIG_ID" > "$PREFIX/build-id"
    popd >/dev/null
fi
if [[ -n "${GITHUB_ENV:-}" ]]; then
    printf 'FFMPEG_DIR=%s\nPKG_CONFIG_PATH=%s/lib/pkgconfig\nLD_LIBRARY_PATH=%s/lib\n' \
        "$PREFIX" "$PREFIX" "$PREFIX" >> "$GITHUB_ENV"
fi
echo "Pinned FFmpeg ready: $PREFIX"
