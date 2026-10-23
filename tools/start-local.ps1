param(
  [int]$ServerPort = 17850,
  [int]$ControllerPort = 18081,
  [int]$ExecutorPort = 18082
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

function Start-StarBridgeProcess([string]$package, [string[]]$arguments) {
  $argumentText = @('run', '-p', $package, '--offline', '--') + $arguments
  $process = Start-Process -FilePath 'cargo' -ArgumentList $argumentText -WorkingDirectory $root -PassThru
  Write-Host "$package started (PID $($process.Id))"
  return $process
}

$serverArguments = @('--listen', "127.0.0.1:$ServerPort")
if (Test-Path 'state/runtime/frps.exe') {
  $serverArguments += @('--frps-path', 'state/runtime/frps.exe')
}
$server = Start-StarBridgeProcess 'starbridge-server' $serverArguments
$deadline = (Get-Date).AddSeconds(20)
do {
  try {
    $health = Invoke-RestMethod "http://127.0.0.1:$ServerPort/api/health" -TimeoutSec 2
    if ($health.service -eq 'starbridge-server') { break }
  } catch { Start-Sleep -Milliseconds 250 }
} while ((Get-Date) -lt $deadline)

if (-not $health) {
  Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue
  throw '中心服务没有在预期时间内启动'
}

$controller = Start-StarBridgeProcess 'starbridge-client' @('--role', 'controller', '--listen', "127.0.0.1:$ControllerPort", '--data-dir', 'state/controller-local', '--no-browser')
$executor = Start-StarBridgeProcess 'starbridge-client' @('--role', 'executor', '--listen', "127.0.0.1:$ExecutorPort", '--data-dir', 'state/executor-local', '--no-browser')
Write-Host '本地实例已启动。控制台地址：'
Write-Host "  controller: http://127.0.0.1:$ControllerPort"
Write-Host "  executor:   http://127.0.0.1:$ExecutorPort"
Write-Host '停止时请结束上面三个 cargo 子进程，或使用任务管理器按 PID 结束。'
