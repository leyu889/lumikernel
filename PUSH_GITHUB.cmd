@echo off
chcp 65001 >nul
setlocal
cd /d "%~dp0"

echo ============================================================
echo   LumiKernel  ->  github.com/leyu889/lumikernel
echo ============================================================
echo.
echo   本脚本会：重建源码快照 -^> 同步 -^> 有变更才提交 -^> 推送
echo   没有变更时直接退出，不会产生空提交。
echo.

where python >nul 2>nul
if errorlevel 1 (
    echo [错误] 找不到 python。请确认 Python 已加入 PATH。
    echo.
    pause
    exit /b 1
)

python tools\push_github.py %*
set RC=%ERRORLEVEL%

echo.
if "%RC%"=="0" (
    echo [完成] 推送流程正常结束。
) else (
    echo [失败] 退出码 %RC%，上面有具体原因。
)
echo.
echo 按任意键关闭窗口...
pause >nul
endlocal
