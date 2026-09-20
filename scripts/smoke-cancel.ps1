# **`cancel`**：一条定义了、方向规则也校验了、但**从来没被产生过**的消息。
#
#   pwsh -File scripts/smoke-cancel.ps1
#
# §2.3 定的语义是「**尽力而为 + 诚实报告**」：
# 与其要求工具保证能停，不如要求它**诚实报告停没停**。
#
# 在这之前超时只是"调用方不等了" —— 工具那边**还在跑**。
# 现在走完整条路：
#   智能体等超时 → 发 cancel 给那个实例 → 服务**确实动手停它** →
#   回一个 `partial` + `canceled`（"我停了，所以它做了一半"）→
#   智能体把这个答复收进上下文，**下一轮模型自己看着办**。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$term = Join-Path $root 'target\debug\harness-term.exe'
$agentBin = Join-Path $root 'target\debug\harness-agent.exe'
function J([string]$p) { return ($p -replace '\\', '\\') }
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-cancel-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null
$sid = 's-cancel'
$dataDir = Join-Path $work 'data'
$scriptPath = Join-Path $work 'script.ndjson'
# 一条**长命令**（ping 30 次 ≈ 30 秒），而调用方的耐心只有 1 秒 ——
# 于是必然触发"等到超时 → 取消"
@'
{"step":"tool","calls":[{"to":"term:1","tool":"open","args":{"path":".","purpose":"试取消"}}]}
{"step":"tool","calls":[{"to":"term:1","tool":"run","args":{"instance_id":"$last.instance_id","cmd":"ping -n 30 127.0.0.1"}}]}
{"step":"text","text":"那条命令被取消了，我改做别的。"}
'@ | Set-Content -LiteralPath $scriptPath -Encoding utf8
$servicesPath = Join-Path $work 'services.json'
@"
[
  { "kind": "term", "id": "term:1", "role": "tool", "wants_fence": true,
    "cmd": ["$(J $term)"] },
  { "kind": "agent", "id": "agent:dada", "role": "agent",
    "bindings": [{"instance": "term:1", "capabilities": ["proc.exec"]}],
    "cmd": ["$(J $agentBin)"],
    "args": ["--persona", "达达", "--script", "$(J $scriptPath)", "--call-timeout-ms", "1000"] }
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
    corr = 'c-1'; sid = $sid; authority = 'user'; payload = @{ task = '跑一条长命令' }
}
$proc.StandardInput.WriteLine(($task | ConvertTo-Json -Compress -Depth 12))
$proc.StandardInput.Flush()
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
$cancel = $records | Where-Object { $_.type -eq 'cancel' }
$canceled = $records | Where-Object { $_.type -eq 'result' -and $_.payload.code -eq 'canceled' } | Select-Object -First 1
Check '1. **`cancel` 真的被发出去了** —— 从智能体发给那个实例' {
    $cancel -ne $null -and $cancel.from -eq 'agent:dada' -and $cancel.to -eq 'term:1'
}
Check '2. 它带着**那条调用的 corr**（不然服务不知道取消哪一条）' {
    $cancel.corr -eq $canceled.corr -and $cancel.corr -ne $null
}
Check '3. 服务**确实动手停了它**，并如实报告 partial + canceled' {
    $canceled -and $canceled.from -eq 'term:1' -and $canceled.payload.effect -eq 'partial' -and `
        $canceled.payload.why -like '*确实停了*'
}
Check '4. 它说得出"做了一半"，而不是含糊的成功或失败' {
    $canceled.payload.remedy -like '*判断它停在哪一步*'
}
Check '5. 这个答复回到了智能体的上下文里（它下一轮能看见）' {
    $item = $records | Where-Object { $_.payload.kind -eq 'model_item' -and $_.payload.item.kind -eq 'tool_result' -and $_.payload.item.text -like '*取消了*' }
    $item -ne $null
}
Check '6. **回合没有因此崩掉**：模型接着把结论给出来了' {
    $answer -and $answer.payload.effect -eq 'confirmed' -and $answer.payload.text.Length -gt 0
}
Check '7. 也没变成"没等回来就报 unverifiable" —— 因为它**确实回了话**' {
    $answer.payload.trace.tool_calls -eq 2
}
Write-Host ''
Write-Host '== 消息流 ==' -ForegroundColor Cyan
$records | Where-Object { $_.corr -ne $null } | ForEach-Object {
    $code = if ($_.payload.code) { $_.payload.code } else { '' }
    Write-Host ("  {0,-8} {1,-9} -> {2,-9} {3}" -f $_.type, $_.from, $_.to, $code)
}
Write-Host ''
Write-Host '== 中控/服务的 stderr ==' -ForegroundColor Cyan
if ($et.IsCompleted) { foreach ($l in ($et.Result -split "`n")) { if ($l.Trim() -ne '' -and $l -like '*cancel*') { Write-Host "  $l" } } }
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "工作目录留在：$work"
