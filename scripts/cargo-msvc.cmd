@echo off
setlocal

set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"

set "PATH=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer;%PATH%"

if exist "%VSWHERE%" (
    for /f "usebackq tokens=*" %%I in (`"%VSWHERE%" -latest -prerelease -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do (
        set "VSINSTALL=%%I"
    )
)

if defined VSINSTALL (
    set "VCVARS=%VSINSTALL%\VC\Auxiliary\Build\vcvars64.bat"
)

if not defined VCVARS (
    for /f "usebackq delims=" %%F in (`dir /b /s /a-d "%ProgramFiles%\Microsoft Visual Studio\vcvars64.bat" "%ProgramFiles(x86)%\Microsoft Visual Studio\vcvars64.bat" 2^>nul`) do (
        if not defined VCVARS set "VCVARS=%%F"
    )
)

if not defined VCVARS (
    echo cargo-msvc: no Visual Studio installation with the C++ x64 tools was found >&2
    exit /b 1
)

if not exist "%VCVARS%" (
    echo cargo-msvc: vcvars64.bat not found at "%VCVARS%" >&2
    exit /b 1
)

call "%VCVARS%" >nul
if errorlevel 1 ( echo cargo-msvc: vcvars64.bat failed >&2 & exit /b 1 )

set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"

cargo %*
exit /b %ERRORLEVEL%
