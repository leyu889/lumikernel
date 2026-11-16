@echo off
chcp 65001 >nul
setlocal
cd /d "%~dp0"
set RUSTUP_HOME=%~dp0tools\rust\rustup
set CARGO_HOME=%~dp0tools\rust\cargo
set PATH=%~dp0tools\rust\cargo\bin;%PATH%

echo === 宿主机单元测试（不含内核二进制）===
cargo test --workspace --exclude lumikernel
if errorlevel 1 goto fail
echo.
echo === 内核编译检查 ===
cargo build -p lumikernel --target x86_64-unknown-none --release
if errorlevel 1 goto fail
echo.
echo 全部通过。
exit /b 0

:fail
echo.
echo 有测试没通过。
pause
exit /b 1
