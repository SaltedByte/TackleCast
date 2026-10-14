#!/usr/bin/env bash
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="$ROOT_DIR/dist"
PKG_NAME=TackleCast-Linux-x86_64
PACKAGE_DIR="$DIST_DIR/$PKG_NAME"
cd "$ROOT_DIR"

bash scripts/setup_linux_dependencies.sh
FFMPEG_DIR="$ROOT_DIR/.deps/ffmpeg"
export PKG_CONFIG_PATH="$FFMPEG_DIR/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
export LD_LIBRARY_PATH="$FFMPEG_DIR/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
echo 'Building current release binary...'
cargo build --release --locked
TARGET_ROOT="${CARGO_TARGET_DIR:-$ROOT_DIR/target}"
[[ "$TARGET_ROOT" = /* ]] || TARGET_ROOT="$ROOT_DIR/$TARGET_ROOT"

# This fixed package path is the only directory the script replaces.
[[ "$PACKAGE_DIR" == "$ROOT_DIR/dist/TackleCast-Linux-x86_64" ]]
rm -rf -- "$PACKAGE_DIR"
mkdir -p "$PACKAGE_DIR/lib" "$PACKAGE_DIR/assets" "$PACKAGE_DIR/licenses"
cp "$TARGET_ROOT/release/tacklecast" "$PACKAGE_DIR/tacklecast"
cp scripts/run.sh "$PACKAGE_DIR/run.sh"
chmod +x "$PACKAGE_DIR/tacklecast" "$PACKAGE_DIR/run.sh"
for library in avcodec avformat avdevice avutil avfilter swresample swscale; do
    cp -a "$FFMPEG_DIR/lib/lib$library.so"* "$PACKAGE_DIR/lib/"
done
cp assets/icon.png "$PACKAGE_DIR/assets/"
cp LICENSE "$PACKAGE_DIR/"
cp "$FFMPEG_DIR/LICENSE.txt" "$PACKAGE_DIR/licenses/FFmpeg-LICENSE.txt"
cp "$ROOT_DIR/.deps/ffmpeg-8.1.3/ffbuild/config.mak" "$PACKAGE_DIR/licenses/FFmpeg-build-config.txt"
cat > "$PACKAGE_DIR/licenses/FFmpeg-source.txt" <<'EOF'
FFmpeg 8.1.3, LGPL 2.1 or later. Source: https://ffmpeg.org/releases/ffmpeg-8.1.3.tar.xz
SHA256: 7138d28c96d9d3e3af4ee3d8cad72741f8ffb40da90c1112235dea3ecd3178a3
Build recipe: scripts/setup_linux_dependencies.sh in TackleCast's source repository.
EOF
cat > "$PACKAGE_DIR/README.txt" <<'EOF'
TackleCast for Linux
Run ./run.sh from this folder. FFmpeg libraries are bundled in lib/.
Official archives target x86_64 Ubuntu 24.04 or newer. Keep the files together.
Your desktop must provide ALSA, glibc, and working Vulkan or OpenGL drivers.
PipeWire/PulseAudio audio uses the desktop's ALSA compatibility plugins.
Press Escape for settings and F11 for fullscreen. Logs are next to the executable.
EOF
python3 - "$PACKAGE_DIR" <<'PY'
import json, pathlib, subprocess, sys, tomllib
destination = pathlib.Path(sys.argv[1])
version = tomllib.loads(pathlib.Path('Cargo.toml').read_text())['package']['version']
commit = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
(destination / 'build-info.json').write_text(json.dumps({
    'version': version, 'commit': commit, 'platform': 'linux-x86_64',
    'ffmpeg_bundled': True, 'ffmpeg_version': '8.1.3',
}, indent=2) + '\n')
PY
python3 scripts/verify_package.py --package "$PACKAGE_DIR"
tar -czf "$DIST_DIR/$PKG_NAME.tar.gz" -C "$DIST_DIR" "$PKG_NAME"
echo "Package ready: $DIST_DIR/$PKG_NAME.tar.gz"
