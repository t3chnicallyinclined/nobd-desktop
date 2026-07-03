@echo off
REM Stop any running test (it locks the exe + keeps driving the pad), rebuild
REM with the latest hm-native, then run. Run from an ELEVATED terminal
REM (device create/remove + OEM registry need admin).
REM
REM The test's create_device now calls remove_devices first, so this converges
REM to exactly ONE "NOBD Controller" no matter how many stacked before.

taskkill /IM nobd-hm-test.exe /F >nul 2>&1
pushd "%~dp0"
cargo build -p hm-native --release --bin nobd-hm-test
if errorlevel 1 ( popd & exit /b 1 )
popd
"%~dp0target\release\nobd-hm-test.exe"
