@echo off
cd /d "%~dp0"

:: Check if setup has been run
if not exist .venv (
    echo Please run setup.bat first.
    pause
    exit /b 1
)
if not exist mpv_bin\libmpv-2.dll (
    echo Please run setup.bat first.
    pause
    exit /b 1
)

:: Launch TackleCast without console window
start "" /B .venv\Scripts\pythonw.exe -m tacklecast
