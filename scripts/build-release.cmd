@echo off
rem LobsterPlus release build (frontend embedded)
rem Usage: scripts\build-release.cmd
rem Output: src-tauri\target\release\lobster-plus.exe
rem NOTE: keep this file ASCII-only. Chinese comments here break under
rem       cmd's default GBK code page when the file is saved as UTF-8.

setlocal
cd /d "%~dp0.."

rem GNU toolchain needs mingw64 dlltool/ld (rustup default is stable-gnu)
set PATH=D:\mingw64\bin;%PATH%

echo [1/2] vite build (frontend dist/)
call npm run build || goto :fail

echo [2/2] cargo build --release --features tauri/custom-protocol
rem custom-protocol embeds dist/ into the binary. Without it the release
rem exe tries to connect to the dev server 127.0.0.1:5177 and shows
rem "cannot reach this page".
rem CFLAGS=-D_STRICT_STDC works around this machine's broken mingw64
rem time.h (missing pthread_time.h) when compiling sqlite3.c.
set CFLAGS=-D_STRICT_STDC
cd src-tauri
cargo build --release --features tauri/custom-protocol || goto :fail

echo.
echo BUILD OK: src-tauri\target\release\lobster-plus.exe
exit /b 0

:fail
echo BUILD FAILED
exit /b 1
