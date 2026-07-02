@echo off
REM Stop any running NOBD app (it locks nobd.exe), rebuild release, relaunch.
REM Run from an ELEVATED terminal — the app runs elevated for the NOBD Controller
REM backend, so killing it also needs elevation.

taskkill /IM nobd.exe /F >nul 2>&1
pushd "%~dp0"
cargo build -p nobd-app --release
if errorlevel 1 ( popd & exit /b 1 )
popd
start "" "%~dp0target\release\nobd.exe"
echo Launched. Left-click the tray icon (teal dot) to open.
