# **绑定**：看得见 vs 够得着，这次它们是一回事了。
#
#   pwsh -File scripts/smoke-binding.ps1
#
# 之前这里有个洞（探针实测过）：**名片管"看得见"，路由管"够得着"，中间没有东西连着**。
# 一个名片里写着 `[]` 的智能体，照样把 `term:1 open` 调成了 ——
# 那条终端的用途字段上写着"我没被授权，但我还是试了"。
#
# 现在三件事各归各位：
#   * **服务**：把 manifest 写完整（工具名 + 说明 + 参数表）—— 它的义务（§11.1）
#   * **中控**：存 manifest、按绑定发工具、路由时检查调用方 —— 它的统筹（§2.3）
#   * **智能体**：不再自带名片，问中控"我被授权用什么"
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$term = Join-Path $root 'target\debug\harness-term.exe'
$echo = Join-Path $root 'target\debug\harness-echo.exe'
$agentBin = Join-Path $root 'target\debug\harness-agent.exe'
function J([string]$p) { return ($p -replace '\\', '\\') }
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-bind-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null
$sid = 's-bind'
$dataDir = Join-Path $work 'data'
$scriptPath = Join-Path $work 'script.ndjson'
@'
{"step":"tool","calls":[{"to":"echo:1","tool":"ping","args":{}}]}
{"step":"tool","calls":[{"to":"term:1","tool":"status","args":{}}]}
{"step":"text","text":"我试了 echo（没成），又用了 term（成了）。"}
'@ | Set-Content -LiteralPath $scriptPath -Encoding utf8
$servicesPath = Join-Path $work 'services.json'
# **只授权 term:1** —— echo:1 活着，但没它的份
@"
[
  { "kind": "term",  "id": "term:1",  "role": "tool",
    "cmd": ["$(J $term)"], "args": ["--fence", "$(J $project)"] },
  { "kind": "echo",  "id": "echo:1",  "role": "tool", "cmd": ["$(J $echo)"] },
  { "kind": "agent", "id": "agent:dada", "role": "agent", "bindings": ["term:1"],
    "cmd": ["$(J $agentBin)"],
    "args": ["--persona", "达达", "--fence", "$(J $project)", "--script", "$(J $scriptPath)"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ctrl
$psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 8000 --host `"$project`""
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
$proc = [System.Diagnostics.Process]::Start($psi)
$script:outTask = $null
function NextLine([int]$timeoutMs) {
    if ($null -eq $script:outTask) { $script:outTask = $proc.StandardOutput.ReadLineAsync() }
    if (-not $script:outTask.Wait($timeoutMs)) { return $null }
    $line = $script:outTask.Result
    $script:outTask = $null
    return $line
}
$task = [ordered]@{
    id = 'm-1'; ts = 0; type = 'request'; from = 'console'; to = 'agent:dada'
    corr = 'c-1'; sid = $sid; authority = 'user'; payload = @{ task = '看看两个工具你能不能都用' }
}
function Write-Line($p, [string]$json) { $proc.StandardInput.WriteLine($json); $proc.StandardInput.Flush() }
Write-Line $proc ($task | ConvertTo-Json -Compress -Depth 12)
$deadline = [DateTime]::UtcNow.AddSeconds(40)
$answer = $null
while ([DateTime]::UtcNow -lt $deadline) {
    $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
    $line = NextLine $left
    if ($null -eq $line) { continue }
    if ($line.Trim() -eq '') { continue }
    $m = $line | ConvertFrom-Json
    if ($m.type -eq 'result' -and $m.corr -eq 'c-1') { $answer = $m; break }
}
$proc.StandardInput.Close()
$proc.WaitForExit(10000) | Out-Null
$et = $proc.StandardError.ReadToEndAsync(); $et.Wait(4000) | Out-Null
$logPath = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl'
$records = @(Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json })
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
Write-Host '== 判据 ==' -ForegroundColor Cyan
$prompt = ($records | Where-Object { $_.payload.kind -eq 'prompt_compiled' } | Select-Object -First 1).payload.system
Check '1. 智能体只看得见被授权的工具：提示词里有 term:1，没有 echo:1' {
    $prompt -like '*term:1*' -and $prompt -notlike '*echo:1*'
}
Check '2. 参数表来自**服务自己的 manifest**（终端那 5 个工具都带说明）' {
    $prompt -like '*open*' -and $prompt -like '*run*' -and $prompt -like '*它的工具*'
}
$reqEcho = $records | Where-Object { $_.type -eq 'request' -and $_.from -eq 'agent:dada' -and $_.to -eq 'echo:1' }
$refuse = $records | Where-Object { $_.to -eq 'agent:dada' -and $_.payload.code -eq 'refused' -and $_.payload.why -like '*没有使用*echo:1*' }
Check '3. **够不着**：调没被授权的 echo:1 ⇒ 被中控拒了' {
    $reqEcho -ne $null -and $refuse -ne $null
}
Check '4. 拒绝是**三段式**的（说清它被授权的是什么）' {
    $refuse.payload.remedy -like '*你被授权的是*term:1*' -and $refuse.payload.remedy -like '*签发*'
}
Check '5. 拒绝带着那次调用的 corr（否则收件人只能猜）' {
    $refuse.corr -ne $null
}
$ok = $records | Where-Object { $_.type -eq 'result' -and $_.from -eq 'term:1' -and $_.payload.effect -eq 'confirmed' }
Check '6. **够得着**：被授权的 term:1 照常能用' { $ok -ne $null }
Check '7. 它还能把回合收掉（拒绝没有把循环打断）' {
    $answer -and $answer.payload.effect -eq 'confirmed' -and $answer.payload.trace.tool_calls -eq 2
}
Write-Host ''
Write-Host '== 智能体看到的工具段（提示词节选）==' -ForegroundColor Cyan
$prompt -split "`n" | Where-Object { $_ -match 'term:1|它的工具|名片' } | ForEach-Object { Write-Host "  $_" }
Write-Host ''
Write-Host '== 那次拒绝 ==' -ForegroundColor Cyan
if ($refuse) { Write-Host "  why:    $($refuse.payload.why)"; Write-Host "  remedy: $($refuse.payload.remedy)" }
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "工作目录留在：$work"
