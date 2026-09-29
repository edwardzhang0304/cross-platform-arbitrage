@echo off
chcp 65001 >nul
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0Read-Diagnostics.ps1" -DataDirectory "%~dp0data"
echo.
pause
