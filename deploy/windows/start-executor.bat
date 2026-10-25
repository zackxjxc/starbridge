@echo off
chcp 65001 >nul
title StarBridge 执行端
cd /d "%~dp0"

if not exist "%~dp0starbridge.exe" (
  echo [错误] 缺少 starbridge.exe，请完整解压便携包。
  pause
  exit /b 1
)

start "" "%~dp0starbridge.exe" --role executor --data-dir "%~dp0state\executor"
exit /b %ERRORLEVEL%
