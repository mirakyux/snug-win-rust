@echo off
rem Install or repair the virtual display driver (Snug's settings page runs the same script).
rem Keep this file ASCII-only: cmd.exe reads .cmd in the system code page, so non-ASCII
rem text here shows up as mojibake in the console window.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0Install-SnugDriver.ps1" -Action Install
if errorlevel 1 (
    echo Installation failed with errorlevel %errorlevel%.
    pause
    exit /b %errorlevel%
)
pause
exit /b 0
