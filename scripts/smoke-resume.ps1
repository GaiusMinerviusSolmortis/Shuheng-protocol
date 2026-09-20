# **跨进程恢复** —— 也是不变量二（可重放）第一次真的被用上。
#
#   pwsh -File scripts/smoke-resume.ps1
#
# 在这之前，"模型可见即已记录"只被检验到"系统提示在日志里"这一层。
# 而实际上智能体往上下文里放六种东西，日志里**只有一种** ——
# 也就是说**日志根本重建不出模型看到的东西**。
# 这个脚本就是那个检验：跑一轮 → 把整个进程停掉 → 冷启动 → 上下文回来没有。
#
# 恢复得回来什么、回不来什么，是这次要说清的事：
#   * **回得来**：会话坐标（状态/托管/点名的智能体）与**模型看到的上下文**
#   * **回不来**：工具实例 —— 它们是进程内的东西，跟着服务一起死了。
#     而"挂起 = 工具实例收掉、会话留着"本来就是这个意思，所以这不是缺陷。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$term = Join-Path $root 'target\debug\harness-term.exe'
$agentBin = Join-Path $root 'target\debug\harness-agent.exe'
function J([string]$p) { return ($p -replace '\\', '\\') }
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-resume-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null
Set-Content -LiteralPath (Join-Path $project 'a.txt') -Value 'hi' -Encoding utf8
$sid = 's-resume'
$dataDir = Join-Path $work 'data'
# （这里曾经写一份 `cards.json`：没人读它 —— 名片只有服务自己的 manifest 一个来源。
#   2026-09-19 对着代码查差距时删掉。）
$scriptA = Join-Path $work 'script-a.ndjson'
@'
{"step":"tool","calls":[{"to":"term:1","tool":"open","args":{"path":".","purpose":"跑测试"}}]}
{"step":"tool","calls":[{"to":"term:1","tool":"run","args":{"instance_id":"$last.instance_id","cmd":"echo FIRST-RUN"}}]}
{"step":"text","text":"第一轮干完了。"}
'@ | Set-Content -LiteralPath $scriptA -Encoding utf8
$scriptB = Join-Path $work 'script-b.ndjson'
Set-Content -LiteralPath $scriptB -Value '{"step":"text","text":"我记着上次的事。"}' -Encoding utf8
$servicesPath = Join-Path $work 'services.json'
@"
[
  { "kind": "term",  "id": "term:1",  "role": "tool",
    "cmd": ["$(J $term)"], "args": ["--fence", "$(J $project)"] },
  { "kind": "agent", "id": "agent:dada", "role": "agent", "bindings": ["term:1"],
    "cmd": ["$(J $agentBin)"],
    "args": ["--persona", "达达", "--fence", "$(J $project)", "--script", "$(J $scriptA)"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
function Start-Ctrl([string[]]$extra) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $ctrl
    $base = @("--sid", $sid, "--data-dir", "`"$dataDir`"", "--services", "`"$servicesPath`"", "--drain-ms", "5000")
    $psi.Arguments = (($base + $extra) -join ' ')
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
    return [System.Diagnostics.Process]::Start($psi)
}
$script:outTask = $null
function Pump($proc, [string]$corr, [int]$timeoutMs = 30000) {
    $deadline = [DateTime]::UtcNow.AddMilliseconds($timeoutMs)
    while ([DateTime]::UtcNow -lt $deadline) {
        # ⚠️ 同一个流上**只能挂一个** ReadLineAsync —— 上一个没读完就再挂，
        # 会直接抛 "The stream is currently in use by a previous operation"。
        if ($null -eq $script:outTask) { $script:outTask = $proc.StandardOutput.ReadLineAsync() }
        $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
        if (-not $script:outTask.Wait($left)) { continue }
        $line = $script:outTask.Result
        $script:outTask = $null
        if ($null -eq $line) { break }
        if ($line.Trim() -eq '') { continue }
        $m = $line | ConvertFrom-Json
        if ($m.type -eq 'result' -and $m.corr -eq $corr) { return $m }
    }
    throw "等 $corr 超时"
}
function Write-Line($proc, [string]$json) {
    $proc.StandardInput.WriteLine($json)
    $proc.StandardInput.Flush()
}
function Task($proc, [string]$corr, [string]$text) {
    $msg = [ordered]@{
        id = "m-$corr"; ts = 0; type = 'request'; from = 'console'; to = 'agent:dada'
        corr = $corr; sid = $sid; authority = 'user'; payload = @{ task = $text }
    }
    Write-Line $proc ($msg | ConvertTo-Json -Compress -Depth 12)
    return Pump $proc $corr
}
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
Write-Host '== 第一轮：正常跑，然后整个进程停掉 ==' -ForegroundColor Cyan
$p1 = Start-Ctrl @()
$r1 = Task $p1 'c-1' '把项目里的东西跑一遍'
$hostMsg = '{"id":"m-host","ts":0,"type":"request","from":"console","to":"ctrl","corr":"c-host","sid":"' + $sid + '","authority":"user","payload":{"op":"host","path":"' + (J $project) + '"}}'
Write-Line $p1 $hostMsg
$null = Pump $p1 'c-host'
$p1.StandardInput.Close()
$p1.WaitForExit(8000) | Out-Null
Check '1. 第一轮正常收尾（它是"上一辈子"的事）' {
    $r1.payload.effect -eq 'confirmed' -and $r1.payload.trace.tool_calls -eq 2
}
$logPath = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl'
$before = @(Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json })
Check '2. **模型看到的六种东西现在都进了日志**（不变量二）' {
    $kinds = @($before | Where-Object { $_.payload.kind -eq 'model_item' } |
        ForEach-Object { $_.payload.item.kind } | Sort-Object -Unique)
    ($kinds -contains 'user') -and ($kinds -contains 'assistant') -and `
        ($kinds -contains 'tool_call') -and ($kinds -contains 'tool_result')
}
Write-Host ''
Write-Host '== 第二轮：冷启动恢复 ==' -ForegroundColor Cyan
$services2 = Join-Path $work 'services-resume.json'
@"
[
  { "kind": "term",  "id": "term:1",  "role": "tool",
    "cmd": ["$(J $term)"], "args": ["--fence", "$(J $project)"] },
  { "kind": "agent", "id": "agent:dada", "role": "agent", "bindings": ["term:1"],
    "cmd": ["$(J $agentBin)"],
    "args": ["--persona", "达达", "--fence", "$(J $project)",
             "--script", "$(J $scriptB)", "--resume"] }
]
"@ | Set-Content -LiteralPath $services2 -Encoding utf8
$servicesPath = $services2
$p2 = Start-Ctrl @('--resume')
Start-Sleep -Milliseconds 1500
try {
    $r2 = Task $p2 'c-2' '你还记得上次干了什么吗？'
} catch {
    $etx = $p2.StandardError.ReadToEndAsync(); $etx.Wait(3000) | Out-Null
    Write-Host '== 第二轮卡住时的 stderr ==' -ForegroundColor Yellow
    if ($etx.IsCompleted) { foreach ($l in ($etx.Result -split "`n")) { if ($l.Trim() -ne '') { Write-Host "  $l" } } }
    throw
}
$p2.StandardInput.Close()
$p2.WaitForExit(8000) | Out-Null
$et2 = $p2.StandardError.ReadToEndAsync(); $et2.Wait(4000) | Out-Null
$after = @(Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json })
$restored = $after | Where-Object { $_.payload.kind -eq 'context_restored' } | Select-Object -Last 1
Check '3. 中控把会话坐标读回来了（状态 / 托管 / 点名的智能体）' {
    $et2.IsCompleted -and $et2.Result -like "*恢复会话 $sid*" -and $et2.Result -like '*托管 1 条*'
}
Check '4. 智能体把自己的上下文要回来了，而且不是空的' {
    $restored -and $restored.payload.items -ge 4
}
Check '5. 系统提示是重新编译的，而且与日志里那份**逐字节一致**' {
    $restored.payload.system_matches -eq $true
}
Check '6. 恢复之后它还能接着干活' {
    $r2.payload.effect -eq 'confirmed' -and $r2.payload.text.Length -gt 0
}
Check '7. **恢复回来的东西里有上一轮的工具结果**（不是个空壳）' {
    # 注意是**任意一条**工具结果 —— 第一条是 open 的返回（里面是实例 JSON），
    # 带命令输出的那条是第二次调用
    $hits = @($before | Where-Object {
        $_.payload.kind -eq 'model_item' -and $_.payload.item.kind -eq 'tool_result' -and `
            $_.payload.item.text -like '*FIRST-RUN*' })
    $hits.Count -ge 1
}
Write-Host ''
Write-Host '== 第二轮（恢复）的 stderr ==' -ForegroundColor Cyan
if ($et2.IsCompleted) { foreach ($l in ($et2.Result -split "`n")) { if ($l.Trim() -ne '') { Write-Host "  $l" } } }
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "工作目录留在：$work"
