@echo off
REM Stop any running NOBD app (it locks nobd.exe), rebuild release, relaunch.
REM Run from an ELEVATED terminal — the app runs elevated for the NOBD Controller
REM backend, so killing it also needs elevation.

REM Debug build — links in seconds (release LTO takes ~1 min). A debug GUI is
REM fine for testing; use `cargo build -p nobd-app --release` for a shipping exe.
taskkill /IM nobd.exe /F >nul 2>&1
pushd "%~dp0"
cargo build -p nobd-app
if errorlevel 1 ( popd & exit /b 1 )
popd
start "" "%~dp0target\debug\nobd.exe"
echo Launched. Left-click the tray icon (teal dot) to open.
