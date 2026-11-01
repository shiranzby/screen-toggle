@echo off
setlocal
if not exist "%~dp0artifacts\sog.exe" (
    echo Rust executable not found. Run build.bat first.
    exit /b 1
)
start "" "%~dp0artifacts\sog.exe" %*
