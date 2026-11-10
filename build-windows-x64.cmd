@echo off
rem Fast x64 Windows build of the SQLLY gpui app.
rem Debug profile + incremental compilation = quickest turnaround.
rem If sccache is on PATH, it is used as the compiler cache so clean/CI
rem rebuilds reuse previously-compiled crates (install: winget install
rem Mozilla.sccache, or: cargo install sccache).
rem Usage: build-windows-x64.cmd [--release]

setlocal
set PROFILE_FLAG=
set PROFILE_DIR=debug
if /i "%~1"=="--release" (
    set PROFILE_FLAG=--release
    set PROFILE_DIR=release
)

rem Opportunistic sccache: only wrap rustc when the binary actually exists.
where sccache >nul 2>nul
if %errorlevel%==0 (
    set RUSTC_WRAPPER=sccache
    rem sccache cannot cache incrementally-compiled crates; disabling
    rem incremental lets the cache serve everything instead.
    set CARGO_INCREMENTAL=0
    echo Using sccache compiler cache.
) else (
    echo sccache not found - building without a compiler cache.
)

pushd "%~dp0client" || exit /b 1

cargo build -p sqlly-gpui --target x86_64-pc-windows-msvc %PROFILE_FLAG%
if errorlevel 1 (
    popd
    exit /b 1
)

if defined RUSTC_WRAPPER sccache --show-stats

echo.
echo Built: %~dp0client\target\x86_64-pc-windows-msvc\%PROFILE_DIR%\SQLLY.exe
popd
endlocal
