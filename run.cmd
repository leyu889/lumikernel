@echo off
chcp 65001 >nul
setlocal
cd /d "%~dp0"

set QEMU=%~dp0tools\qemu\qemu-system-x86_64.exe
if not exist "%QEMU%" set QEMU=qemu-system-x86_64.exe

if not exist "target\x86_64-unknown-none\release\lumikernel" (
  echo 还没编译，先跑 build.cmd
  pause
  exit /b 1
)

echo 启动 LumiKernel（串口输出，Ctrl+C 退出）...
"%QEMU%" -kernel "target\x86_64-unknown-none\release\lumikernel" ^
  -m 512M -serial stdio -display none -no-reboot -no-shutdown
