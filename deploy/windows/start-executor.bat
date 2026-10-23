@echo off
chcp 65001 >nul
title StarBridge 执行端
cd /d "%~dp0"

set ROLE=executor
set PORT=18082
set DATA_DIR=executor-data

if not exist "%DATA_DIR%\config.json" (
  mkdir "%DATA_DIR%" 2>nul
  > "%DATA_DIR%\config.json" (
    echo {
    echo   "server_url": "https://starbridge.zackxjxc.de5.net",
    echo   "display_name": "%COMPUTERNAME%-%ROLE%",
    echo   "ssh_alias": "%COMPUTERNAME%-%ROLE%",
    echo   "ssh_user": "%USERNAME%",
    echo   "allow_relay": true,
    echo   "direct_only": false
    echo }
  )
)

echo 控制台地址: http://127.0.0.1:%PORT%
"%~dp0starbridge.exe" --role %ROLE% --listen 127.0.0.1:%PORT% --data-dir %DATA_DIR%
echo.
echo StarBridge 已退出，退出码 %ERRORLEVEL%。
pause
