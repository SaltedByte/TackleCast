@echo off
echo ===========================
echo  TackleCast Setup
echo ===========================
echo.
cd /d "%~dp0"

:: Find Python - check common locations
set PYTHON=
where python >nul 2>&1 && set PYTHON=python
if not defined PYTHON (
    if exist "%LOCALAPPDATA%\Programs\Python\Python312\python.exe" (
        set PYTHON="%LOCALAPPDATA%\Programs\Python\Python312\python.exe"
    ) else if exist "%LOCALAPPDATA%\Programs\Python\Python313\python.exe" (
        set PYTHON="%LOCALAPPDATA%\Programs\Python\Python313\python.exe"
    ) else (
        echo ERROR: Python not found. Please install Python 3.12+ from python.org
        pause
        exit /b 1
    )
)

:: Create virtual environment
if not exist .venv (
    echo [1/4] Creating virtual environment...
    %PYTHON% -m venv .venv
) else (
    echo [1/4] Virtual environment exists.
)

:: Install dependencies
echo [2/4] Installing Python dependencies...
.venv\Scripts\pip.exe install -r requirements.txt -q

:: Download mpv if needed
if not exist mpv_bin\libmpv-2.dll (
    echo [3/4] Downloading mpv (libmpv)...
    mkdir mpv_bin 2>nul

    powershell -NoProfile -Command ^
        "$url = (Invoke-RestMethod 'https://api.github.com/repos/shinchiro/mpv-winbuild-cmake/releases/latest').assets | Where-Object { $_.name -match 'mpv-dev-x86_64.*\.7z$' -and $_.name -notmatch 'v3' } | Select-Object -First 1 -ExpandProperty browser_download_url; " ^
        "Write-Host \"Downloading from $url\"; " ^
        "Invoke-WebRequest -Uri $url -OutFile mpv-dev.7z"

    :: Extract libmpv-2.dll
    if exist "C:\Program Files\7-Zip\7z.exe" (
        "C:\Program Files\7-Zip\7z.exe" x -ompv_bin -y mpv-dev.7z libmpv-2.dll >nul
    ) else (
        echo ERROR: 7-Zip is required to extract mpv. Please install from https://7-zip.org
        pause
        exit /b 1
    )
    del mpv-dev.7z 2>nul
) else (
    echo [3/4] mpv already installed.
)

:: Build launcher exe
if not exist TackleCast.exe (
    echo [4/4] Building TackleCast.exe...
    .venv\Scripts\pip.exe install pyinstaller -q
    .venv\Scripts\pyinstaller.exe --onefile --noconsole --name TackleCast --icon assets\icon.ico --distpath . launcher.py >nul 2>&1
    rmdir /s /q build 2>nul
    del TackleCast.spec 2>nul
) else (
    echo [4/4] TackleCast.exe exists.
)

echo.
echo ===========================
echo  Setup complete!
echo  Double-click TackleCast.exe to launch.
echo ===========================
pause
