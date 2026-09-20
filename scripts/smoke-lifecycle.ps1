# **中控的生命周期管控**：探针 + "最后活动时间" + `stop`。
#
#   pwsh -File scripts/smoke-lifecycle.ps1
#
# 这一轮补的是中控那层缺的三样，而且**刻意都不做自动动作**：
#
#   1. **探针**（`health`，服务自己在 manifest 里声明的那个入口）——
#      TCP keepalive 的先例：它默认关闭、阈值按用途分，因为**它本来就不可靠**。
#      所以我们更保守一层：探到了不做什么，**探不到也不做什么**。
#   2. **"最后活动时间"** —— §5.20 的原话："中控上的'最后活动时间'
#      **只是给你看的，不是任何自动动作的触发**"。
#   3. **`stop`** —— 四条确定回收路径里"用户手动收"在**服务级**的入口
#      （在这之前只有 `spawn` 没有 `stop`）。
#
# 最要紧的两条判据是"反着"的：**装死的服务不会被收掉**、**闲置的实例一直留着**。
# 它们钉的不是功能，是**决定**（§5.20：默认「开着就是有用」）。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$term = Join-Path $root 'target\debug\harness-term.exe'
$echo = Join-Path $root 'target\debug\harness-echo.exe'
$agentBin = Join-Path $root 'target\debug\harness-agent.exe'
function J([string]$p) { return ($p -replace '\\', '\\') }
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-life-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null
$sid = 's-life'
$dataDir = Join-Path $work 'data'
$scriptPath = Join-Path $work 'script.ndjson'
Set-Content -LiteralPath $scriptPath -Value '{"step":"text","text":"我在。"}' -Encoding utf8
$servicesPath = Join-Path $work 'services.json'
# 三个服务里有一个是**装死**的（报完到就挂住、再也不读 stdin）——
# 它演的是"进程还在，但不应答"，和"崩了"（EOF）完全是两回事
@"
[
  { "kind": "term",  "id": "term:1",     "role": "tool",
    "cmd": ["$(J $term)"], "args": ["--fence", "$(J $project)"] },
  { "kind": "echo",  "id": "echo:1",     "role": "tool", "cmd": ["$(J $echo)"] },
  { "kind": "echo",  "id": "echo:zombie","role": "tool", "cmd": ["$(J $echo)"], "args": ["--freeze"] },
  { "kind": "agent", "id": "agent:dada", "role": "agent", "bindings": ["term:1"],
    "cmd": ["$(J $agentBin)"],
    "args": ["--persona", "达达", "--fence", "$(J $project)", "--script", "$(J $scriptPath)"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ctrl
# **tick 调快**（400ms）免得验收跑几分钟；探针超时 = 2 × tick = 800ms
$psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 3000 --tick-ms 400"
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
$proc = [System.Diagnostics.Process]::Start($psi)
$script:outTask = $null
$replies = @{}
function Pump([string[]]$want, [int]$timeoutSec = 20) {
    $deadline = [DateTime]::UtcNow.AddSeconds($timeoutSec)
    while ([DateTime]::UtcNow -lt $deadline) {
        if (@($want | Where-Object { -not $replies.ContainsKey($_) }).Count -eq 0) { return }
        if ($null -eq $script:outTask) { $script:outTask = $proc.StandardOutput.ReadLineAsync() }
        if (-not $script:outTask.Wait(200)) { continue }
        $line = $script:outTask.Result
        $script:outTask = $null
        if ($null -eq $line -or $line.Trim() -eq '') { continue }
        $m = $line | ConvertFrom-Json
        if ($m.type -eq 'result' -and $m.corr) { $replies[$m.corr] = $m }
    }
}
function Ask($body, [string]$corr, [string]$to = 'ctrl') {
    $m = [ordered]@{ id = "m-$corr"; ts = 0; type = 'request'; from = 'console'; to = $to
                     corr = $corr; sid = $sid; authority = 'user'; payload = $body }
    $proc.StandardInput.WriteLine(($m | ConvertTo-Json -Compress -Depth 12))
    $proc.StandardInput.Flush()
    Pump @($corr) 20
    return $replies[$corr]
}
function Graph([string]$corr) {
    $r = Ask @{ op = 'list' } $corr
    return $r.payload.data
}

Write-Host '== 等三个服务上线，然后开一个终端实例 ==' -ForegroundColor Cyan
Start-Sleep -Milliseconds 1200
$open = Ask @{ tool = 'open'; args = @{ path = '.'; purpose = '生命周期验收' } } 'o' 'term:1'
$tid = $open.payload.instance_id
$list1 = Graph 'l1'
Write-Host '== 什么都不碰，等几个 tick（看探针和"最后活动时间"）==' -ForegroundColor Cyan
Start-Sleep -Milliseconds 2500
$list2 = Graph 'l2'
Start-Sleep -Milliseconds 2500
$list3 = Graph 'l3'
Write-Host '== 碰它一下（`status` 指名要这个实例）==' -ForegroundColor Cyan
$null = Ask @{ tool = 'read'; args = @{ instance_id = $tid } } 'touch' 'term:1'
Start-Sleep -Milliseconds 300
$list4 = Graph 'l4'
Write-Host '== `stop` 三种对象 ==' -ForegroundColor Cyan
$s1 = Ask @{ op = 'stop'; instance = 'echo:zombie' } 's1'
Start-Sleep -Milliseconds 1200
$list5 = Graph 'l5'
$s2 = Ask @{ op = 'stop'; instance = 'agent:dada' } 's2'
$s3 = Ask @{ op = 'stop'; instance = 'ctrl' } 's3'
$proc.StandardInput.Close()
$proc.WaitForExit(8000) | Out-Null
$et = $proc.StandardError.ReadToEndAsync(); $et.Wait(4000) | Out-Null
$logPath = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl'
$records = @(Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json })
$indexPath = Join-Path (Join-Path $dataDir 'index') 'instances.jsonl'
$index = @(Get-Content -LiteralPath $indexPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json })

function Node($list, [string]$id) { return ($list | Where-Object { $_.id -eq $id } | Select-Object -First 1) }
$zombie = Node $list3.instances 'echo:zombie'
$healthy = Node $list3.instances 'echo:1'
$term3 = Node $list3.instances 'term:1'
$t3 = ($list3.tool_instances | Where-Object { $_.id -eq $tid } | Select-Object -First 1)
$t4 = ($list4.tool_instances | Where-Object { $_.id -eq $tid } | Select-Object -First 1)
$t5 = ($list5.tool_instances | Where-Object { $_.id -eq $tid } | Select-Object -First 1)
$probes = @($records | Where-Object { $_.type -eq 'request' -and $_.from -eq 'ctrl' -and $_.payload.tool -eq 'ping' })
$unres = @($records | Where-Object { $_.payload.code -eq 'service_unresponsive' })

$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
Write-Host '== 判据 ==' -ForegroundColor Cyan
Check '1. **探针真的发出去了**，而且用的是服务自己声明的 `health` 入口' {
    $probes.Count -ge 3 -and (($probes | ForEach-Object { $_.to }) -contains 'term:1') -and `
        (($probes | ForEach-Object { $_.to }) -contains 'echo:1')
}
Check '2. 应答的服务 `unresponsive = false`，而且"最后听见它"是新鲜的' {
    $healthy.unresponsive -eq $false -and $healthy.idle_ms -lt 2000 -and `
        $term3.unresponsive -eq $false -and $term3.idle_ms -lt 2000
}
Check '3. **装死的那个被判成"不应答"**（进程还在，只是不回话）' {
    $zombie -and $zombie.unresponsive -eq $true -and $unres.Count -ge 1
}
Check '4. 那条事件说清了"不一定死了"（不替它下结论）' {
    $unres[0].payload.remedy -like '*不一定死了*' -and $unres[0].payload.remedy -like '*stop*'
}
Check '5. **探到了不做动作、探不到也不做**：装死的服务还挂在 `list` 里（§5.20）' {
    $zombie -ne $null -and $zombie.state -ne 'offline'
}
Check '6. **"最后活动时间"会自己往前走**（那个数只给人看）' {
    $t3 -and $t3.idle_ms -ge 1500
}
Check '7. **碰它一下，那个数就归零**（指名要实例的调用算活动）' {
    $t4 -and $t4.idle_ms -lt $t3.idle_ms -and $t4.idle_ms -lt 1500
}
Check '8. **闲置不触发任何动作**：实例一直留着，服务也还在' {
    $t5 -ne $null -and $t5.id -eq $tid -and (Node $list5.instances 'term:1').state -ne 'offline'
}
Check '9. `stop` **真的收掉了一个服务**：它从图上消失了' {
    $s1.payload.effect -eq 'confirmed' -and (Node $list5.instances 'echo:zombie') -eq $null
}
Check '10. 而且这笔账进了身份索引（`exited`）' {
    ($index | Where-Object { $_.id -eq 'echo:zombie' -and $_.event -eq 'exited' }).Count -ge 1
}
Check '11. `stop` **一个智能体被拒**，并指出去哪停它（会话管生命周期）' {
    $s2.payload.effect -eq 'refused' -and $s2.payload.remedy -like '*suspend*' -and `
        $s2.payload.remedy -like '*end*'
}
Check '12. `stop` 中控自己的入口也被拒（那是锯管子）' {
    $s3.payload.effect -eq 'refused' -and $s3.payload.why -like '*入口*'
}
Check '13. **敕令里带的是服务自己写的遗嘱**（不是中控编的）' {
    $inj = $records | Where-Object { $_.type -eq 'inject' -and $_.payload.reason -eq 'service_down' -and `
        $_.payload.instance -eq 'echo:zombie' } | Select-Object -First 1
    $inj -and $inj.payload.text -like '*只是个 mock*' -and $inj.payload.text -notlike '*套话*'
}
<#
  第 14/15 条是 **2026-09-20 审计**补的 —— 它们盯的是一个"看着接好了其实没接上"的地方：

  中控的探针回包，**曾经 100% 被判成孤儿结果**。原因是两张纸的顺序：
  `on_service_line` 先 `pending.remove(corr)`、再 `route`；而 `route` 的方向规则四
  判"这条结果是不是中控要过的"，**查的正是 `pending`** —— 摘完了才问，当然说没有。
  代价：探针的答复被丢掉（`route` 遇违规直接 return），还每轮生一条假故障。

  这一条试过：修之前 18 条探针回包 → 18 条孤儿故障；修之后 0 条。
  判据写在这里，是为了让"先摘后发"这个写法再也回不来。
#>
$toCtrl = @($records | Where-Object { $_.to -eq 'ctrl' -and $_.type -eq 'result' })
$orphan = @($records | Where-Object { $_.payload.why -like '*孤儿结果*' })
Check '14. **探针的回包没有被误判成孤儿结果**' {
    $toCtrl.Count -ge 3 -and $orphan.Count -eq 0
}
Check '15. 服务掉线时它名下未决的调用**逐个被交代过**（含中控自己那笔探针），且没生孤儿' {
    $sent = @($records | Where-Object { $_.from -eq 'ctrl' -and $_.type -eq 'request' } | ForEach-Object { $_.corr })
    $settled = @($records | Where-Object { $_.payload.code -eq 'service_down' })
    $toSelf = @($settled | Where-Object { $_.to -eq 'ctrl' })
    # `ctrl → ctrl` 那条（把"探针的答复"交代给自己）正是**第二个踩坑点**：
    # 服务掉线时"逐个交代"也曾经先摘账再 route，于是同样被判成孤儿。
    # 所以这一条要指名道姓地查：**这些 crr 里，没有一个被 fault 成孤儿。**
    # （第 14 条查的是总数，这条查的是"第二处写法有没有再犯"。）
    $selfCorrs = @($toSelf | ForEach-Object { $_.corr })
    $blamed = @($records | Where-Object { $_.payload.why -like '*孤儿结果*' } |
        Where-Object { $selfCorrs -contains $_.corr })
    $toCtrl.Count -gt 0 -and
        (@($toCtrl | Where-Object { $sent -notcontains $_.corr }).Count -eq 0) -and
        ($settled.Count -ge 1) -and ($toSelf.Count -ge 1) -and ($blamed.Count -eq 0)
}

Write-Host ''
Write-Host '== 图上三个服务的状态（含给人看的那两样）==' -ForegroundColor Cyan
foreach ($n in ($list3.instances | Where-Object { $_.kind -ne 'ctrl' -and $_.kind -ne 'channel' })) {
    Write-Host ("  {0,-14} unresponsive={1,-6} idle_ms={2}" -f $n.id, $n.unresponsive, $n.idle_ms)
}
Write-Host ''
Write-Host '== 实例的"最后活动时间"（三次快照）==' -ForegroundColor Cyan
Write-Host ("  {0,-8} {1,8} {2,8} {3,8}" -f '实例', 't1', 't2', 't3')
Write-Host ("  {0,-8} {1,8} {2,8} {3,8}" -f $tid, $t3.idle_ms, $t4.idle_ms, $t5.idle_ms)
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "工作目录留在：$work"
