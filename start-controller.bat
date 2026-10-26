@echo off
chcp 65001 >nul
title StarBridge 设备中心
cd /d "%~dp0"

if not exist "state\runtime\frpc.exe" echo [警告] 缺少 state\runtime\frpc.exe，只能测试控制面。

cargo build --locked -p starbridge-client --bins
if errorlevel 1 exit /b 1

cargo run -p starbridge-client -- --data-dir "state\controller"
if errorlevel 1 (
  echo.
  echo StarBridge 启动失败，请检查上方错误。
  pause
)
