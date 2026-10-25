@echo off
chcp 65001 >nul
title StarBridge 执行端
cd /d "%~dp0"

if not exist "state\runtime\frpc.exe" echo [警告] 缺少 state\runtime\frpc.exe，只能测试控制面。

cargo run -p starbridge-client -- --role executor --data-dir "state\executor"
if errorlevel 1 (
  echo.
  echo StarBridge 启动失败，请检查上方错误。
  pause
)
