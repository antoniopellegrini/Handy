@echo off
setlocal

set "HANDY_VS_INSTALLER=C:\Program Files (x86)\Microsoft Visual Studio\Installer"
set "HANDY_VSDEVCMD=C:\Program Files\Microsoft Visual Studio\2022\Community\Common7\Tools\VsDevCmd.bat"
set "HANDY_LLVM_BIN=C:\Program Files\LLVM\bin"

if not exist "%HANDY_VSDEVCMD%" (
  echo Visual Studio Developer Command Prompt not found:
  echo %HANDY_VSDEVCMD%
  exit /b 1
)

if not exist "%HANDY_LLVM_BIN%\clang.exe" (
  echo Clang not found:
  echo %HANDY_LLVM_BIN%\clang.exe
  exit /b 1
)

set "PATH=%HANDY_VS_INSTALLER%;%PATH%"
call "%HANDY_VSDEVCMD%" -arch=arm64 -host_arch=amd64
if errorlevel 1 exit /b %ERRORLEVEL%

set "PATH=%HANDY_LLVM_BIN%;%PATH%"

where bun >nul 2>&1
if errorlevel 1 (
  echo Bun not found in PATH.
  exit /b 1
)

pushd "%~dp0"
bun run tauri dev %*
set "HANDY_EXIT_CODE=%ERRORLEVEL%"
popd

exit /b %HANDY_EXIT_CODE%
