@echo off
rem Build helper for this host: Rust is installed without rustup, so cargo cannot
rem find the MSVC linker by itself. vcvars64.bat must run in the same shell first.
rem
rem Usage:  build.bat                  (debug build)
rem         build.bat --release        (release build)
rem         build.bat test             (cargo test)
rem
rem From an MSYS bash shell invoke it as:
rem   MSYS2_ARG_CONV_EXCL='*' cmd.exe /c build.bat --release

setlocal
set "VS_BUILD_TOOLS=C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools"

if not exist "%VS_BUILD_TOOLS%\VC\Auxiliary\Build\vcvars64.bat" (
    echo build.bat: vcvars64.bat not found under "%VS_BUILD_TOOLS%"
    exit /b 2
)

call "%VS_BUILD_TOOLS%\VC\Auxiliary\Build\vcvars64.bat" >nul 2>&1
if errorlevel 1 (
    echo build.bat: vcvars64.bat failed
    exit /b 2
)

if /i "%~1"=="test" (
    cargo test %2 %3 %4 %5
) else if /i "%~1"=="clean" (
    rem Also needed after moving the working tree: cached fingerprints carry the old paths.
    cargo clean
) else (
    cargo build %*
)
exit /b %ERRORLEVEL%
