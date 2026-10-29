@echo off
setlocal DisableDelayedExpansion
if not exist "%~dp0trustd.exe" (
  echo Extract the complete trustd archive before starting INSTALL.cmd.
  pause
  exit /b 1
)
start "" "%~dp0trustd.exe" maintenance
