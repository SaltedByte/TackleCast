"""Lightweight launcher for TackleCast.

PyInstaller compiles this into TackleCast.exe which:
1. Activates the venv
2. Launches the app with no console window
"""
import os
import sys
import subprocess


def main():
    app_dir = os.path.dirname(os.path.abspath(sys.argv[0]))
    venv_python = os.path.join(app_dir, ".venv", "Scripts", "pythonw.exe")

    if not os.path.exists(venv_python):
        # Try setup first
        setup_bat = os.path.join(app_dir, "setup.bat")
        if os.path.exists(setup_bat):
            subprocess.run([setup_bat], cwd=app_dir, shell=True)
        if not os.path.exists(venv_python):
            import ctypes
            ctypes.windll.user32.MessageBoxW(
                0,
                "Please run setup.bat first to install dependencies.",
                "TackleCast",
                0x10,
            )
            sys.exit(1)

    mpv_dll = os.path.join(app_dir, "mpv_bin", "libmpv-2.dll")
    if not os.path.exists(mpv_dll):
        import ctypes
        ctypes.windll.user32.MessageBoxW(
            0,
            "mpv not found. Please run setup.bat first.",
            "TackleCast",
            0x10,
        )
        sys.exit(1)

    subprocess.Popen(
        [venv_python, "-m", "tacklecast"],
        cwd=app_dir,
        creationflags=0x08000000,  # CREATE_NO_WINDOW
    )


if __name__ == "__main__":
    main()
