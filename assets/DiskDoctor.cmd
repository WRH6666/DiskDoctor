@echo off
rem ============================================================
rem  DiskDoctor - double-click THIS file to start.
rem
rem  Why this file exists:
rem  Windows does not run a .ps1 on double-click (it opens in
rem  Notepad, or the execution policy blocks it). A .cmd can be
rem  double-clicked, so this only forwards to the PowerShell
rem  launcher sitting next to it.
rem
rem  Keep this file ASCII-only on purpose:
rem  .cmd is parsed using the console code page, so non-ASCII text
rem  in here turns to garbage on a machine with a different code
rem  page. All Chinese UI text lives in the .ps1 instead.
rem ============================================================

setlocal
cd /d "%~dp0"

if not exist "%~dp0DiskDoctor.ps1" (
  echo.
  echo   [ERROR] DiskDoctor.ps1 is missing.
  echo           Keep all extracted files in the same folder.
  echo.
  pause
  exit /b 1
)

if not exist "%~dp0diskdoctor.exe" (
  echo.
  echo   [ERROR] diskdoctor.exe is missing.
  echo           Keep all extracted files in the same folder.
  echo.
  pause
  exit /b 1
)

powershell -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0DiskDoctor.ps1" %*
set "RC=%ERRORLEVEL%"

if not "%RC%"=="0" (
  echo.
  echo   [ERROR] Launcher exited with code %RC%.
  echo           The message above should explain why.
  echo.
  pause
)
