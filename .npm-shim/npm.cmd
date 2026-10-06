@echo off
setlocal
set "ROOT=D:\WishProject\WorkbuddySwitchAPi\wt-port"
if /I "%1"=="run" goto :runscript
echo [npm-shim] unsupported npm invocation: %* 1>&2
exit /b 1

:runscript
set "SCRIPT=%2"
if /I "%SCRIPT%"=="build" goto :build
if /I "%SCRIPT%"=="tauri" goto :tauri
echo [npm-shim] unsupported npm script: %SCRIPT% 1>&2
exit /b 1

:build
cd /d "%ROOT%"
node "%ROOT%\node_modules\typescript\bin\tsc"
if errorlevel 1 exit /b %errorlevel%
node "%ROOT%\node_modules\vite\bin\vite.js" build
exit /b %errorlevel%

:tauri
cd /d "%ROOT%"
shift
shift
if "%~1"=="--" shift
set "ARGS="
:collect
if "%~1"=="" goto :runt
set "ARGS=%ARGS% %1"
shift
goto :collect
:runt
node "%ROOT%\node_modules\@tauri-apps\cli\tauri.js"%ARGS%
exit /b %errorlevel%
