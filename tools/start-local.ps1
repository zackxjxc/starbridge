param(
  [int]$ServerPort = 17850,
  [Alias("ControllerPort")] [int]$NodeAPort = 18081,
  [Alias("ExecutorPort")] [int]$NodeBPort = 18082
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

& cargo build --locked --offline -p starbridge-client --bins
if ($LASTEXITCODE -ne 0) { throw 'Client build failed' }

function Start-StarBridgeProcess([string]$package, [string[]]$arguments) {
  $argumentText = @('run', '-p', $package, '--offline', '--') + $arguments
  $process = Start-Process -FilePath 'cargo' -ArgumentList ($argumentText -join ' ') -WindowStyle Hidden -WorkingDirectory $root -PassThru
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

$nodeA = Start-StarBridgeProcess 'starbridge-client' @('--listen', "127.0.0.1:$NodeAPort", '--data-dir', 'state/controller-local', '--no-browser')
$nodeB = Start-StarBridgeProcess 'starbridge-client' @('--listen', "127.0.0.1:$NodeBPort", '--data-dir', 'state/executor-local', '--no-browser')
Write-Host '本地实例已启动。控制台地址：'
Write-Host "  device A: http://127.0.0.1:$NodeAPort"
Write-Host "  device B:   http://127.0.0.1:$NodeBPort"
Write-Host '停止时请结束上面三个 cargo 子进程，或使用任务管理器按 PID 结束。'
