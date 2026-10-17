@echo off
setlocal EnableExtensions
set "SCRIPT_DIR=%~dp0"
cd /d "%SCRIPT_DIR%..\.."

rem Ignite Kindle external release packager (standalone profile).
rem Usage:
rem   scripts\kindle\package-release.bat
rem   scripts\kindle\package-release.bat --version 0.9.8
rem   scripts\kindle\package-release.bat --skip-build

set "VERSION="
set "SKIP_BUILD="

:parse
if "%~1"=="" goto run
if /i "%~1"=="--version" (
  if "%~2"=="" (
    echo package-release: --version requires a value
    exit /b 2
  )
  set "VERSION=%~2"
  shift
  shift
  goto parse
)
if /i "%~1"=="--skip-build" (
  set "SKIP_BUILD=1"
  shift
  goto parse
)
echo package-release: unknown argument %~1
echo usage: scripts\kindle\package-release.bat [--version X.Y.Z] [--skip-build]
exit /b 2

:run
if defined VERSION (
  if defined SKIP_BUILD (
    powershell -NoProfile -ExecutionPolicy Bypass -File "%SCRIPT_DIR%package-release.ps1" -Version "%VERSION%" -SkipBuild
  ) else (
    powershell -NoProfile -ExecutionPolicy Bypass -File "%SCRIPT_DIR%package-release.ps1" -Version "%VERSION%"
  )
) else (
  if defined SKIP_BUILD (
    powershell -NoProfile -ExecutionPolicy Bypass -File "%SCRIPT_DIR%package-release.ps1" -SkipBuild
  ) else (
    powershell -NoProfile -ExecutionPolicy Bypass -File "%SCRIPT_DIR%package-release.ps1"
  )
)
set "ERR=%ERRORLEVEL%"
if not "%ERR%"=="0" (
  echo package-release: failed with exit %ERR%
  exit /b %ERR%
)
exit /b 0
