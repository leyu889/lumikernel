@echo off
chcp 65001 >nul
setlocal
cd /d "%~dp0"
set RUSTUP_HOME=%~dp0tools\rust\rustup
set CARGO_HOME=%~dp0tools\rust\cargo
set PATH=%~dp0tools\rust\cargo\bin;%PATH%

echo [1/2] 编译内核 ...
cargo build -p lumikernel --target x86_64-unknown-none --release
if errorlevel 1 goto fail

echo [2/2] 完成: target\x86_64-unknown-none\release\lumikernel
exit /b 0

:fail
echo 编译失败，请把上面的错误发给我。
pause
exit /b 1
