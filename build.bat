@echo off
setlocal
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0checks\build_release.ps1"
exit /b %errorlevel%
