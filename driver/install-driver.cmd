@echo off
rem 手动安装/修复虚拟显示器驱动（由 Snug 的设置页调用时也会走同一个脚本）。
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0Install-SnugDriver.ps1" -Action Install
if errorlevel 1 (
    echo Installation failed with errorlevel %errorlevel%.
    pause
    exit /b %errorlevel%
)
pause
exit /b 0
