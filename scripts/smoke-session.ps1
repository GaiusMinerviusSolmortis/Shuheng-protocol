# 内核最后两层：**通道**与**会话**。
#
#   pwsh -File scripts/smoke-session.ps1
#
# 这一轮补的是那张图上唯一空着的一层（会话），以及"接口"那层：
#
#   * 入口原来是**写死的常量**（`console`，15 处引用，钉死在 stdin 上）——
#     现在它是个**实例**：有自己的 id、自己的权威上限、自己的连接状态；
#   * 会话原来只是一个 sid + 一个目录 —— 现在它有状态、托管、点名的智能体、
#     参与者，而且**挂起 / 恢复 / 结束**分得开。
#
# 顺带第一次真的触发了 §1.1 那条"权威只准降不准升"：
# 一条自称 `user` 的消息，从一条上限是 `agent` 的通道进来 —— 它会被降级。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$term = Join-Path $root 'target\debug\harness-term.exe'
$agentBin = Join-Path $root 'target\debug\harness-agent.exe'
function J([string]$p) { return ($p -replace '\\', '\\') }
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-session-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null
Set-Content -LiteralPath (Join-Path $project 'a.txt') -Value 'hi' -Encoding utf8
$sid = 's-session'
$dataDir = Join-Path $work 'data'
# （这里曾经写一份 `cards.json` —— 那是"名片由智能体自带"那个年代的残留，
#   **没有任何人读它**：名片现在只有服务自己的 manifest 一个来源（§11.1）。
#   2026-09-19 对着代码查差距时发现的，删掉。）
$scriptPath = Join-Path $work 'script.ndjson'
Set-Content -LiteralPath $scriptPath -Value '{"step":"text","text":"我在。"}' -Encoding utf8
$servicesPath = Join-Path $work 'services.json'
@"
[
  { "kind": "term",  "id": "term:1",  "role": "tool",
    "cmd": ["$(J $term)"], "args": ["--fence", "$(J $project)"] },
  { "kind": "agent", "id": "agent:dada", "role": "agent", "bindings": ["term:1"],
    "cmd": ["$(J $agentBin)"],
    "args": ["--persona", "达达", "--fence", "$(J $project)",
             "--script", "$(J $scriptPath)"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ctrl
# **通道是配置出来的**：这条口子叫「微信」，权威上限只有 agent（转发来的消息）
$psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 6000 --channel 微信:agent"
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
$proc = [System.Diagnostics.Process]::Start($psi)
$script:outTask = $null
$script:seq = 0
$script:replies = @{}
function NextLine([int]$timeoutMs) {
    if ($null -eq $script:outTask) { $script:outTask = $proc.StandardOutput.ReadLineAsync() }
    if (-not $script:outTask.Wait($timeoutMs)) { return $null }
    $line = $script:outTask.Result
    $script:outTask = $null
    return $line
}
function Send([string]$corr, [hashtable]$payload, [string]$authority = 'user') {
    $script:seq++
    $msg = [ordered]@{
        id = "m-$($script:seq)"; ts = 0; type = 'request'; from = '微信'
        to = 'ctrl'; corr = $corr; sid = $sid; authority = $authority; payload = $payload
    }
    $proc.StandardInput.WriteLine(($msg | ConvertTo-Json -Compress -Depth 12))
    $proc.StandardInput.Flush()
}
function Wait([string]$corr, [int]$timeoutMs = 20000) {
    if ($script:replies.ContainsKey($corr)) { return $script:replies[$corr] }
    $deadline = [DateTime]::UtcNow.AddMilliseconds($timeoutMs)
    while ([DateTime]::UtcNow -lt $deadline) {
        $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
        $line = NextLine $left
        if ($null -eq $line) { continue }
        if ($line.Trim() -eq '') { continue }
        $m = $line | ConvertFrom-Json
        if ($m.type -eq 'result') { $script:replies[$m.corr] = $m }
        if ($m.type -eq 'result' -and $m.corr -eq $corr) { return $m }
    }
    throw "等 $corr 超时"
}
function Ask([string]$corr, [hashtable]$payload) { Send $corr $payload; return Wait $corr }
function Session([string]$corr) { return (Ask $corr @{ op = 'session' }).payload.data }
function Graph([string]$corr) { return (Ask $corr @{ op = 'list' }).payload.data }
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
Write-Host '== 判据 ==' -ForegroundColor Cyan
# 先等两个服务自报完
Start-Sleep -Milliseconds 800
$g0 = Graph 'g-0'
$ch = $g0.instances | Where-Object { $_.kind -eq 'channel' }
Check '1. **入口是个实例**：图上有通道节点，而且带着它的权威上限（agent，不是 user）' {
    $ch -and $ch.id -eq '微信' -and $ch.ceiling -eq 'agent' -and $ch.state -eq 'connected'
}
$s0 = Session 's-0'
Check '2. 会话是一等对象：有状态、有参与者' {
    $s0.sid -eq $sid -and $s0.state -eq 'active' -and ($s0.participants -contains '微信')
}
$r = Ask 'g-host' @{ op = 'host'; path = $project }
$s1 = Session 's-1'
Check '3. 托管是一条**会话内的动作**（§7.1），记进了会话' {
    $r.payload.effect -eq 'confirmed' -and $s1.hosts.Count -eq 1 -and $s1.hosts[0].path -like '*project*'
}
Check '4. 托管也落进了 meta.json（会话是持久化的）' {
    $meta = Get-Content -LiteralPath (Join-Path (Join-Path $dataDir 'sessions') (Join-Path $sid 'meta.json')) -Raw | ConvertFrom-Json
    $meta.hosts.Count -eq 1 -and $meta.state -eq 'active'
}
$d = Ask 'g-delegate' @{ op = 'delegate'; agent = 'agent:dada' }
$s2 = Session 's-2'
Check '5. **会话点名智能体**（§12.7），并把它记进参与者' {
    $d.payload.effect -eq 'confirmed' -and $s2.agent -eq 'agent:dada' -and ($s2.participants -contains 'agent:dada')
}
Check '6. 点名一个不存在的智能体要拒绝，并说清现在有谁' {
    $bad = Ask 'g-bad' @{ op = 'delegate'; agent = 'agent:ghost' }
    $bad.payload.code -eq 'instance_gone' -and $bad.payload.remedy -like '*agent:dada*'
}
$null = Ask 'g-susp' @{ op = 'suspend' }
$s3 = Session 's-3'
$g3 = Graph 'g-3'
Check '7. **挂起 = 实例收掉、会话留着**：状态是 suspended，工具实例全没了' {
    $s3.state -eq 'suspended' -and $g3.tool_instances.Count -eq 0
}
# ⚠️ 日志是**带缓冲**的，只在落盘屏障（会话结束）上才刷干净 ——
# 所以所有"读日志"的判据都放到进程退出之后（下面 #8 / #10）。
$null = Ask 'g-res' @{ op = 'resume' }
Start-Sleep -Milliseconds 900
$s4 = Session 's-4'
$g4 = Graph 'g-4'
Check '9. **恢复 = 照同一批 id 重新装配**（不是换一套新的）' {
    $s4.state -eq 'active' -and (($g4.instances | Where-Object { $_.id -eq 'term:1' }) -ne $null) -and `
        (($g4.instances | Where-Object { $_.id -eq 'agent:dada' }) -ne $null)
}
$e = Ask 'g-end' @{ op = 'end' }
Check '10. 结束是结束（它让整个会话收摊）' { $e.payload.data.state -eq 'ended' }
$proc.WaitForExit(8000) | Out-Null
$et = $proc.StandardError.ReadToEndAsync(); $et.Wait(4000) | Out-Null
$logPath = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl'
$records = @(Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json })
Check '11. 挂起时那几条敕令**进了日志**（"敕令是写给日志的"，进程退出前才刷盘）' {
    $inj = @($records | Where-Object { $_.type -eq 'inject' -and $_.payload.reason -eq 'session_recycle' })
    $inj.Count -ge 1
}
Check '12. **§1.1 第一次真的被触发**：自称 user 的消息从 agent 上限的通道进来，被降级' {
    $fromCh = @($records | Where-Object { $_.from -eq '微信' -and $_.type -eq 'request' })
    ($fromCh.Count -ge 1) -and (($fromCh | Where-Object { $_.authority -ne 'agent' }).Count -eq 0)
}
Check '13. 会话结束后 meta 里记的是 ended（归档留痕）' {
    $meta2 = Get-Content -LiteralPath (Join-Path (Join-Path $dataDir 'sessions') (Join-Path $sid 'meta.json')) -Raw | ConvertFrom-Json
    $meta2.state -eq 'ended'
}
Write-Host ''
Write-Host '== 中控的 stderr ==' -ForegroundColor Cyan
if ($et.IsCompleted) { foreach ($l in ($et.Result -split "`n")) { if ($l.Trim() -ne '') { Write-Host "  $l" } } }
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "工作目录留在：$work"
