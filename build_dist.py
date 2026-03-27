"""Build script for creating a standalone TackleCast distribution.

Run: python build_dist.py
Output: dist/TackleCast/ (folder ready to zip and distribute)
"""
import PyInstaller.__main__
import shutil
import os

ROOT = os.path.dirname(os.path.abspath(__file__))
DIST = os.path.join(ROOT, "dist", "TackleCast")

print("Building TackleCast standalone distribution...")

PyInstaller.__main__.run([
    os.path.join(ROOT, "tacklecast", "__main__.py"),
    "--name", "TackleCast",
    "--noconsole",
    "--icon", os.path.join(ROOT, "assets", "icon.ico"),
    # Include the tacklecast package
    "--add-data", f"{os.path.join(ROOT, 'tacklecast')};tacklecast",
    # Include assets
    "--add-data", f"{os.path.join(ROOT, 'assets')};assets",
    # Include mpv DLL
    "--add-binary", f"{os.path.join(ROOT, 'mpv_bin', 'libmpv-2.dll')};.",
    # Output to dist/TackleCast
    "--distpath", os.path.join(ROOT, "dist"),
    # Overwrite
    "--noconfirm",
    # Hidden imports that PyInstaller might miss
    "--hidden-import", "tacklecast",
    "--hidden-import", "tacklecast.app",
    "--hidden-import", "tacklecast.capture",
    "--hidden-import", "tacklecast.audio",
    "--hidden-import", "tacklecast.devices",
    "--hidden-import", "tacklecast.overlay",
    "--hidden-import", "tacklecast.settings",
    "--hidden-import", "sounddevice",
    "--hidden-import", "numpy",
    "--hidden-import", "imageio_ffmpeg",
])

# Clean up build artifacts
for f in ["build", "TackleCast.spec"]:
    p = os.path.join(ROOT, f)
    if os.path.isdir(p):
        shutil.rmtree(p)
    elif os.path.isfile(p):
        os.remove(p)

# Verify output
exe = os.path.join(DIST, "TackleCast.exe")
if os.path.exists(exe):
    size_mb = os.path.getsize(exe) / 1024 / 1024
    total = sum(
        os.path.getsize(os.path.join(dp, f))
        for dp, _, fns in os.walk(DIST)
        for f in fns
    ) / 1024 / 1024
    print(f"\nBuild complete!")
    print(f"  EXE: {size_mb:.1f} MB")
    print(f"  Total folder: {total:.0f} MB")
    print(f"  Output: {DIST}")
    print(f"\nTo distribute: zip the dist/TackleCast folder")
else:
    print("ERROR: Build failed - exe not found")
