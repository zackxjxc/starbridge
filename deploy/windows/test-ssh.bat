@echo off
chcp 65001 >nul
title StarBridge SSH 测试
cd /d "%~dp0"
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0test-ssh.ps1" %*
echo.
pause
