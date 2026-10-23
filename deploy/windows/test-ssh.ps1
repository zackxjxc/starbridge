# StarBridge 端到端 SSH 测试：登录控制端和执行端、建立连接，再通过隧道执行 ssh 和 scp。
param(
  [int]$ControllerPort = 18081,
  [int]$ExecutorPort = 18082,
  [string]$Username,
  # 不传时交互输入，避免密码留在命令历史里。
  [string]$Password,
  [ValidateSet('direct_preferred', 'direct_only', 'relay_only')]
  [string]$Policy = 'direct_preferred'
)

$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$root = $PSScriptRoot
$sessionId = $null

function Step([string]$text) { Write-Host "`n== $text" -ForegroundColor Cyan }
function Ok([string]$text) { Write-Host "  [OK] $text" -ForegroundColor Green }

function Invoke-Api([int]$port, [string]$method, [string]$path, $body = $null) {
  $params = @{ Uri = "http://127.0.0.1:$port$path"; Method = $method; UseBasicParsing = $true; TimeoutSec = 60 }
  if ($null -ne $body) {
    $params.Body = [Text.Encoding]::UTF8.GetBytes(($body | ConvertTo-Json -Compress))
    $params.ContentType = 'application/json'
  }
  try {
    $response = Invoke-WebRequest @params
  } catch {
    $raw = $_.Exception.Response
    if ($null -eq $raw) { throw }
    $text = (New-Object IO.StreamReader($raw.GetResponseStream(), [Text.Encoding]::UTF8)).ReadToEnd()
    try { $text = ($text | ConvertFrom-Json).error } catch {}
    throw "$method $path 返回 $([int]$raw.StatusCode)：$text"
  }
  # 接口返回 UTF-8 但不带 charset，按 UTF-8 解码避免中文乱码。
  $text = [Text.Encoding]::UTF8.GetString($response.RawContentStream.ToArray())
  if ($text) { return $text | ConvertFrom-Json }
}

function Wait-Until([string]$what, [int]$seconds, [scriptblock]$check) {
  $deadline = (Get-Date).AddSeconds($seconds)
  while ((Get-Date) -lt $deadline) {
    $result = & $check
    if ($result) { return $result }
    Start-Sleep -Seconds 1
  }
  throw "等待超时（${seconds} 秒）：$what"
}

function Get-State([int]$port) {
  try { Invoke-Api $port GET /api/v1/state } catch { $null }
}

function Ensure-Instance([int]$port, [string]$role) {
  $state = Get-State $port
  if (-not $state) {
    $script = Join-Path $root "start-$role.bat"
    if (-not (Test-Path $script)) { throw "127.0.0.1:$port 上没有运行 $role，也找不到 $script" }
    Write-Host "  启动 $role ..."
    Start-Process -FilePath $script -WorkingDirectory $root | Out-Null
    $state = Wait-Until "$role 启动" 60 { Get-State $port }.GetNewClosure()
  }
  if ($state.role -ne $role) { throw "127.0.0.1:$port 上运行的是 $($state.role)，不是 $role" }
  Ok "$role 已运行：http://127.0.0.1:$port"
  $state
}

function Invoke-Native([string]$exe, [string[]]$argv) {
  # 原生程序写 stderr 时不能触发 Stop，否则拿不到退出码和报错原文。
  $previous = $ErrorActionPreference
  $ErrorActionPreference = 'Continue'
  try { $output = & $exe @argv 2>&1 | ForEach-Object { "$_" } } finally { $ErrorActionPreference = $previous }
  [pscustomobject]@{ Code = $LASTEXITCODE; Output = ($output -join "`n").Trim() }
}

try {
  Step '检查本机实例'
  $null = Ensure-Instance $ControllerPort 'controller'
  $null = Ensure-Instance $ExecutorPort 'executor'

  Step '登录中心'
  if (-not $Username) { $Username = Read-Host '账户名称' }
  if (-not $Password) {
    $secure = Read-Host '密码' -AsSecureString
    $Password = [Runtime.InteropServices.Marshal]::PtrToStringBSTR(
      [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure))
  }
  $credential = @{ username = $Username; password = $Password }
  # 控制端先登记公钥，执行端登录时就能拿到并写入 authorized_keys。
  $controller = Invoke-Api $ControllerPort POST /api/v1/login $credential
  $executor = Invoke-Api $ExecutorPort POST /api/v1/login $credential
  $Password = $null; $credential = $null
  $controllerId = $controller.node_id
  $executorId = $executor.node_id
  Ok "已登录 $($controller.server_url)，账户 $Username"

  Step '检查 frpc 能否运行'
  # 端侧返回的是相对其工作目录的路径；直接交给 & 会被当成“模块\命令”解析。
  $frpcPath = $controller.runtime.frpc_path
  if (-not [IO.Path]::IsPathRooted($frpcPath)) { $frpcPath = Join-Path $root $frpcPath }
  if (-not (Test-Path $frpcPath)) { throw "找不到 frpc：$frpcPath" }
  $frpc = Invoke-Native $frpcPath @('--version')
  if ($frpc.Code -ne 0) {
    throw "frpc 无法运行（$frpcPath）：$($frpc.Output)。多半是 Windows Defender 拦截，请在保护历史记录里允许，或把本目录加入排除项。"
  }
  Ok "frpc $($frpc.Output)"

  Step '检查 SSH 准备情况'
  Write-Host "  执行端：$($executor.ssh.message)"
  if (-not $executor.ssh.port) { throw "执行端内置 SSH 服务没有启动：$($executor.ssh.message)" }
  $null = Wait-Until '执行端收到控制端公钥（中心是旧版时不会转发公钥）' 30 {
    $s = Get-State $ExecutorPort
    $s.devices | Where-Object { $_.node_id -eq $controllerId -and $_.ssh_public_key }
  }.GetNewClosure()
  Ok '执行端已信任控制端公钥'
  $target = Wait-Until '控制端拿到执行端主机公钥' 30 {
    $s = Get-State $ControllerPort
    $s.devices | Where-Object { $_.node_id -eq $executorId -and $_.online -and $_.ssh_host_key }
  }.GetNewClosure()
  Ok "目标执行端 $($target.display_name)，登录用户 $($target.ssh_user)，权限 $($target.ssh_privilege)"

  Step "建立连接（$Policy）"
  if ((Get-State $ControllerPort).session) { $null = Invoke-Api $ControllerPort DELETE /api/v1/disconnect }
  $connected = Invoke-Api $ControllerPort POST /api/v1/connect @{ target_node_id = $executorId; policy = $Policy }
  # 控制端 frpc 被拦截或启动失败时，错误只出现在这次响应里，下一次心跳就会被覆盖。
  if (-not $connected.session -or $connected.stage -match 'failed|blocked|missing') {
    throw "控制端连接失败（$($connected.stage)）：$($connected.last_error)"
  }
  $sessionId = $connected.session.session_id
  Write-Host "  会话 $sessionId"
  $null = Wait-Until '执行端接管会话（执行端每 10 秒轮询一次）' 40 {
    $s = Get-State $ExecutorPort
    if ($s.stage -match 'failed|blocked|missing') { throw "执行端失败：$($s.last_error)" }
    $s.session -and $s.session.session_id -eq $sessionId -and $s.session.status -eq 'connected'
  }.GetNewClosure()
  $state = Get-State $ControllerPort
  if ($state.session.status -ne 'connected') { throw "控制端会话状态 $($state.session.status)：$($state.last_error)" }
  if ($state.ssh_command -notmatch '^ssh -F "([^"]+)" (\S+)$') { throw "控制端没有给出 SSH 命令：$($state.ssh_command)" }
  $config = $Matches[1]
  $alias = $Matches[2]
  Ok "两端已连接，SSH 命令：$($state.ssh_command)"

  $ssh = @("$root\runtime\ssh.exe", "$root\state\runtime\ssh.exe") | Where-Object { Test-Path $_ } | Select-Object -First 1
  $scp = if ($ssh) { Join-Path (Split-Path $ssh) 'scp.exe' } else { 'scp' }
  if (-not $ssh) { $ssh = 'ssh' }
  $common = @('-F', $config, '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=15')

  Step '通过隧道执行 SSH'
  # 直连打洞可能要十几秒才完成，前几次失败属于正常现象。
  for ($i = 1; $i -le 5; $i++) {
    $result = Invoke-Native $ssh ($common + @($alias, 'whoami'))
    if ($result.Code -eq 0) { break }
    Write-Host "  第 $i 次失败：$($result.Output)" -ForegroundColor Yellow
    if ($result.Output -match 'Permission denied|Host key verification failed') { break }
    Start-Sleep -Seconds 5
  }
  if ($result.Code -ne 0) { throw "ssh 失败：$($result.Output)" }
  Ok "远端 whoami：$($result.Output)"

  Step '通过隧道传输文件'
  $work = Join-Path ([IO.Path]::GetTempPath()) "starbridge-test-$([guid]::NewGuid().ToString('N').Substring(0, 8))"
  New-Item -ItemType Directory $work | Out-Null
  Push-Location $work
  try {
    # 用相对路径，避免 scp 把 C: 当成主机名。
    $name = "$(Split-Path $work -Leaf).txt"
    $content = "starbridge $(Get-Date -Format o)"
    Set-Content -Path up.txt -Value $content -Encoding ASCII
    $up = Invoke-Native $scp ($common + @('up.txt', "${alias}:$name"))
    if ($up.Code -ne 0) { throw "scp 上传失败：$($up.Output)" }
    $down = Invoke-Native $scp ($common + @("${alias}:$name", 'down.txt'))
    $null = Invoke-Native $ssh ($common + @($alias, "del $name"))
    if ($down.Code -ne 0) { throw "scp 下载失败：$($down.Output)" }
    if ((Get-Content down.txt -Raw).Trim() -ne $content) { throw '下载内容与上传内容不一致' }
    Ok 'scp 上传、下载内容一致，远端临时文件已删除'
  } finally {
    Pop-Location
    Remove-Item -Recurse -Force $work
  }
  Ok "会话路径：$((Get-State $ControllerPort).session.path)"

  Step '测试通过，下面这段可以交给 AI'
  $limit = if ($target.ssh_privilege -eq 'user') {
    '普通用户权限：不能安装系统软件、修改系统设置或访问其他用户的文件'
  } else {
    "权限：$($target.ssh_privilege)"
  }
  $handoff = @"
你可以通过 SSH 控制一台 Windows 执行端 $($target.display_name)：
- 执行命令：ssh -F "$config" -o BatchMode=yes $alias "<命令>"
- 上传：scp -F "$config" -o BatchMode=yes <本地文件> ${alias}:<远端路径>；下载时把两个参数对调
- 远端默认 shell 是 cmd.exe，复杂操作用 powershell -NoProfile -Command "..."
- 登录用户 $($target.ssh_user)，$limit
- 会话 10 分钟后过期，需要我在控制端重新连接
- 连接失败时不要反复重试或猜测，把报错原样告诉我
"@
  Write-Host $handoff
  try { Set-Clipboard -Value $handoff; Ok '已复制到剪贴板' } catch {}
} catch {
  Write-Host "`n[失败] $($_.Exception.Message)" -ForegroundColor Red
  $logs = @("$root\executor-data\ssh\sshd.log", "$root\state\executor-local\ssh\sshd.log")
  if ($sessionId) {
    foreach ($dir in 'controller-data', 'executor-data', 'state\controller-local', 'state\executor-local') {
      $logs += "$root\$dir\routes\$sessionId.log"
    }
  }
  $ansi = "$([char]27)\[[0-9;]*m"
  foreach ($log in $logs | Where-Object { Test-Path $_ }) {
    Write-Host "`n--- $log（最后 8 行）"
    Get-Content $log -Tail 8 -Encoding UTF8 | ForEach-Object { $_ -replace $ansi, '' }
  }
  exit 1
}
