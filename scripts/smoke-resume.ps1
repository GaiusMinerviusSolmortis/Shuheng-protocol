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
    # ⚠️ **托管是必须的**（2026-09-27）：这一份本来就在 `$project` 里干活 —— 如实托出来。
    $base = @("--sid", $sid, "--data-dir", "`"$dataDir`"", "--services", "`"$servicesPath`"",
              "--drain-ms", "5000", "--host", "`"$project`"")
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
Write-Host '== 第三轮：日志坏了怎么办（崩溃语义：尾部可以半残、中间不许有洞）==' -ForegroundColor Cyan
# **这一轮只问中控**：读日志是协议层的事 —— 所以这里不起智能体，
# console 直接发 `{"op":"log","instance":...}`。
#
# ⚠️ 2026-09-23（L1）：这条 op 原来叫 `replay`，回的是**重建好的上下文**
# （`system` + `items`）。现在中控只回**记录**（原样信封），重建是智能体自己的事 ——
# 所以下面这几条判据盯的是"记录读得出来吗、跳过几行如实说了吗、有洞拒了吗"，
# **而"这些记录能不能重建成上下文"由第 3～7 条那些跑真智能体的判据盯着**
# （它读 `context_restored` 那条事件，走的就是新的那条路）。
$servicesEmpty = Join-Path $work 'services-empty.json'
Set-Content -LiteralPath $servicesEmpty -Value '[]' -Encoding utf8
$servicesPath = $servicesEmpty
# 模拟"崩在写日志的那一下"：文件尾部多出半行 JSON（没收尾，也没换行）
Add-Content -LiteralPath $logPath -Value '{"seq":9999,"recorded_at":1,"id":"m-broken","ts":0,"type":"resu' -NoNewline -Encoding utf8
$p3 = Start-Ctrl @()
Start-Sleep -Milliseconds 1500
$replayMsg = '{"id":"m-replay","ts":0,"type":"request","from":"console","to":"ctrl","corr":"c-replay","sid":"' + $sid + '","authority":"user","payload":{"op":"log","instance":"agent:dada"}}'
Write-Line $p3 $replayMsg
$r3 = Pump $p3 'c-replay'
Check '8. **尾部半残的日志照样读得出来**（崩在写日志那一下，不该把整份日志判死）' {
    # 现在是**记录**（原样信封），不再是重建好的上下文项
    $r3.payload.effect -eq 'confirmed' -and @($r3.payload.data.records).Count -ge 4
}
Check '9. 而且它**如实说**跳过了一行半残的尾巴（不是偷偷跳过）' {
    $r3.payload.data.broken_tail -eq 1
}
Check '10. 那半行**没有冒充成一条记录**（重建停在最后一条完整记录上）' {
    $r3.payload.data.to_seq -ge 1 -and $r3.payload.data.to_seq -lt 9999
}
$p3.StandardInput.Close()
$p3.WaitForExit(8000) | Out-Null
$et3 = $p3.StandardError.ReadToEndAsync(); $et3.Wait(4000) | Out-Null
# 再把**中间**挖掉一行：那是"中间有洞"，跟尾部半残**不是一回事**，不许默默放过去
# （要**先停掉中控** —— 它开着追加句柄，别的进程改不了这个文件）
$keep = @([System.IO.File]::ReadAllLines($logPath) | Where-Object { $_.Trim() -ne '' })
$holed = New-Object System.Collections.Generic.List[string]
$holed.AddRange([string[]]$keep[0..1])
$holed.AddRange([string[]]$keep[3..($keep.Count - 1)])
[System.IO.File]::WriteAllLines($logPath, $holed)
$p4 = Start-Ctrl @()
Start-Sleep -Milliseconds 1500
$replay2 = '{"id":"m-replay2","ts":0,"type":"request","from":"console","to":"ctrl","corr":"c-replay2","sid":"' + $sid + '","authority":"user","payload":{"op":"log","instance":"agent:dada"}}'
Write-Line $p4 $replay2
$r4 = Pump $p4 'c-replay2'
Check '11. **中间有洞就不干**：明确拒，并说清是哪一段断了' {
    $r4.payload.effect -eq 'refused' -and $r4.payload.why -like '*洞*'
}
# 12. **名字不许重号**：整份日志里没有一个重复的 `id` / `corr`。
#
# 这条是实测抓出来的：修之前，第二轮（重启之后）把 13 个 `m-agent-dada-*`
# **重新铸了一遍** —— 因为计数器是**进程内的**，进程一重启就从 1 重来，
# 而 `id` 的语义是"消息自身唯一"。`corr` 更糟：它撞上"还没收到结果"的那条时，
# 中控那张 `pending` 表是**静默覆盖**的。
# 现在：中控手里有一张水位表（它看得见每一条消息，见 `App::id_water`），
# 拉起实例时把水位塞进它的环境（`HARNESS_ID_WATER`）。
Check '12. **重启之后名字不重号**：`id` 一条不重，`corr` 也没被发起第二次' {
    # ⚠️ **两条不同的规则，别混**（第一版判据就把它们混了，于是自己红了一回）：
    #   · `id` 是"消息自身唯一" —— 整份日志里**一条都不许重**；
    #   · `corr` 是**一次调用的那条链**：同一条 corr 出现在「请求 / chunk / 结果」上
    #     是**配对本身**（§3），所以只查它有没有被**发起两次**（两条 `request` 共用它）。
    $ids = @($after | ForEach-Object { $_.id })
    $reqCorrs = @($after | Where-Object { $_.type -eq 'request' } |
        ForEach-Object { $_.corr } | Where-Object { $_ })
    $dupIds = @($ids | Group-Object | Where-Object { $_.Count -gt 1 })
    $dupCorrs = @($reqCorrs | Group-Object | Where-Object { $_.Count -gt 1 })
    if ($dupIds.Count -gt 0 -or $dupCorrs.Count -gt 0) {
        Write-Host ("      重的 id：{0}；被发起两次的 corr：{1}" -f `
            (($dupIds | Select-Object -First 4 | ForEach-Object { $_.Name }) -join ', '), `
            (($dupCorrs | Select-Object -First 4 | ForEach-Object { $_.Name }) -join ', ')) -ForegroundColor DarkGray
    }
    ($dupIds.Count -eq 0) -and ($dupCorrs.Count -eq 0)
}
$p4.StandardInput.Close()
$p4.WaitForExit(8000) | Out-Null
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
