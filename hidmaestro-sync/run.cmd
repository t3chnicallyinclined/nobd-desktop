@echo off
REM Dev run helper — stops any prior instance (which locks the exe) BEFORE
REM building, so `dotnet run` never fails on a file lock. Run from an ELEVATED
REM terminal (InstallDriver + OEM-name branding need admin; killing the prior
REM elevated instance also needs admin).
REM
REM   run.cmd --window 5
REM   run.cmd --latency 400
REM
REM A hard-killed instance skips its OEM-name Clear(), but the next launch's
REM HMOemNameOverride.RecoverOrphans() restores it — safe.

taskkill /IM hidmaestro-sync.exe /F >nul 2>&1
dotnet run --project "%~dp0HidMaestroSync.csproj" -c Release -- %*
