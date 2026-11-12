#!/usr/bin/env bash
# Build the trimmed, static, LGPL FFmpeg that the standalone Windows release
# links into TackleCast.exe. Run from an MSYS2 shell (setup_windows_dependencies.ps1
# -StaticFfmpeg does this for you).
#
#   build_static_ffmpeg.sh <source.tar.xz> <install-prefix> <gcc|msvc>
#
# Only what TackleCast uses is enabled: DirectShow capture, the MJPEG and raw
# video decoders (plus H.264/HEVC for cards that offer only compressed modes),
# and swscale. No network, no external libraries, no GPL code.
set -euo pipefail

source_archive=$(cygpath -u "$1")
prefix=$(cygpath -u "$2")
toolchain=$3

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
tar -xf "$source_archive" -C "$work" --strip-components=1
cd "$work"

args=(
    --prefix="$prefix"
    --enable-static
    --disable-shared
    --disable-programs
    --disable-doc
    --disable-network
    # Nothing is picked up from the build machine by accident.
    --disable-autodetect
    --enable-w32threads
    --disable-everything
    --enable-indev=dshow
    --enable-decoder=mjpeg,rawvideo,h264,hevc
    --enable-parser=mjpeg,h264,hevc
    --pkg-config-flags=--static
)
case $toolchain in
    msvc)
        # Match Rust's static CRT (+crt-static in .cargo/config.toml).
        args+=(--toolchain=msvc --extra-cflags=-MT)
        ;;
    gcc) ;;
    *)
        echo "unknown toolchain '$toolchain' (expected gcc or msvc)" >&2
        exit 2
        ;;
esac
if command -v nasm >/dev/null; then
    echo "Using $(nasm -v)"
else
    # Works, but MJPEG and colour conversion lose their SIMD fast paths.
    echo "warning: nasm not found; building without x86 assembly" >&2
    args+=(--disable-x86asm)
fi

./configure "${args[@]}"
make -j"$(nproc)"
make install

if [[ $toolchain == msvc ]]; then
    # FFmpeg names MSVC static libraries libNAME.a; rustc looks for NAME.lib.
    for archive in "$prefix"/lib/lib*.a; do
        name=$(basename "$archive" .a)
        cp "$archive" "$prefix/lib/${name#lib}.lib"
    done
fi
cp COPYING.LGPLv2.1 "$prefix/LICENSE.txt"
echo "Static FFmpeg installed to $prefix"
