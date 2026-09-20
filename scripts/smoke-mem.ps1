# **沉淀式记忆**的验收（08-记忆 §16）。
#
#   pwsh -File scripts/smoke-mem.ps1
#
# 这一层要证的事，和别的服务不太一样 —— 它**不碰你的文件**，
# 所以"围栏"那条线在这里退场，真正要证的是**它会不会骗人**：
#
#   · 同一条话写两遍，它是认出来，还是攒出第二条？
#   · `if_none_match` 说"没变"的时候，它是不是真的没给你内容？
#   · 过期的碎片是**被删了**，还是"不给你但数出来"？
#   · `forget` 是把行删了（那就没人知道曾经有过它），还是立了个墓碑？
#   · 换一个进程、换一次会话，账还在不在？
#
# 顺带证两件架构上的事：**能力校验在服务侧**（`mem.write` / `mem.read` /
# `mem.curate`），以及**一处碎片库只许一个进程写**（域锁 ⇒ degraded）。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$mem = Join-Path $root 'target\debug\harness-mem.exe'
$agentBin = Join-Path $root 'target\debug\harness-agent.exe'
foreach ($b in @($ctrl, $mem, $agentBin)) { if (-not (Test-Path $b)) { throw "产物不在：$b" } }
function J([string]$p) { return ($p -replace '\\', '\\') }
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-mem-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$store = Join-Path $work 'memory'
New-Item -ItemType Directory -Path $store -Force | Out-Null
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null
# 锚点用"这个项目"—— 谁给锚点是调用方的事，服务不猜（§16.5）
$anchor = $project

function Start-Ctrl([string]$sid, [string]$tag, [string]$bindings) {
    $dataDir = Join-Path $work "data-$tag"
    $scriptPath = Join-Path $work "script-$tag.ndjson"
    # 智能体只做一件事：把"我记一条"翻译成一次 deposit —— 用来验**能力**那条线
    @'
{"step":"tool","calls":[{"to":"mem:1","tool":"deposit","args":{"anchor":"__ANCHOR__","text":"智能体自己写下的一条","epistemic":"decision"}}]}
{"step":"text","text":"写完了。"}
'@ -replace '__ANCHOR__', (J $anchor) | Set-Content -LiteralPath $scriptPath -Encoding utf8
    $servicesPath = Join-Path $work "services-$tag.json"
    @"
[
  { "kind": "mem", "id": "mem:1", "role": "tool",
    "cmd": ["$(J $mem)"], "args": ["--store", "$(J $store)", "--default-ttl-ms", "0"] },
  { "kind": "agent", "id": "agent:dada", "role": "agent", "bindings": [$bindings],
    "cmd": ["$(J $agentBin)"],
    "args": ["--persona", "达达", "--fence", "$(J $project)", "--script", "$(J $scriptPath)"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $ctrl
    $psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 5000 --tick-ms 0"
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
    $proc = [System.Diagnostics.Process]::Start($psi)
    return [pscustomobject]@{ proc = $proc; sid = $sid; dataDir = $dataDir; outTask = $null }
}

function NextLineOf($h, [int]$timeoutMs) {
    if ($null -eq $h.outTask) { $h.outTask = $h.proc.StandardOutput.ReadLineAsync() }
    if (-not $h.outTask.Wait($timeoutMs)) { return $null }
    $line = $h.outTask.Result
    $h.outTask = $null
    return $line
}
$script:seq = 0
function Send($h, [string]$to, [string]$corr, [hashtable]$payload, [string]$from = 'console') {
    $script:seq++
    $m = [ordered]@{ id = "m-$($script:seq)"; ts = 0; type = 'request'; from = $from
                     to = $to; corr = $corr; sid = $h.sid; authority = 'user'; payload = $payload }
    $h.proc.StandardInput.WriteLine(($m | ConvertTo-Json -Compress -Depth 12))
    $h.proc.StandardInput.Flush()
}
function WaitCorr($h, [string]$corr, [int]$timeoutMs = 25000) {
    $deadline = [DateTime]::UtcNow.AddMilliseconds($timeoutMs)
    while ([DateTime]::UtcNow -lt $deadline) {
        $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
        $line = NextLineOf $h $left
        if ($null -eq $line) { continue }
        if ($line.Trim() -eq '') { continue }
        $m = $line | ConvertFrom-Json
        if ($m.type -eq 'result' -and $m.corr -eq $corr) { return $m }
    }
    return $null
}
function Stop-Ctrl($h) {
    $h.proc.StandardInput.Close()
    $h.proc.WaitForExit(8000) | Out-Null
    try { $d = $h.proc.StandardOutput.ReadToEndAsync(); $d.Wait(2000) | Out-Null } catch { }
    $e = $h.proc.StandardError.ReadToEndAsync(); $e.Wait(3000) | Out-Null
    if ($e.IsCompleted) { return $e.Result } else { return '' }
}

Write-Host '== 场景 A：这一层会不会骗人 ==' -ForegroundColor Cyan
# 智能体先只拿 `mem.read` —— **它能碰到那个实例，但写不了**（第 13 条要用）
$A = Start-Ctrl 's-mem' 'a' '{"instance": "mem:1", "capabilities": ["mem.read"]}'
$Aerr = ''
# 等服务起来
for ($i = 0; $i -lt 20; $i++) {
    Start-Sleep -Milliseconds 150
    Send $A 'ctrl' "ready-$i" @{ op = 'list' }
    $r = WaitCorr $A "ready-$i" 800
    if ($r -and (@($r.payload.data.instances | ForEach-Object { $_.id }) -contains 'mem:1')) { break }
}

Send $A 'mem:1' 'd1' @{ tool = 'deposit'; args = @{ anchor = $anchor; text = '我们决定用方案 X'; epistemic = 'decision'; tags = @('decision') } }
$d1 = WaitCorr $A 'd1'
Send $A 'mem:1' 'd2' @{ tool = 'deposit'; args = @{ anchor = $anchor; text = '我们决定用方案 X'; epistemic = 'decision' } }
$d2 = WaitCorr $A 'd2'
# 第三条**必须换个文本** —— 否则幂等那条会把 id 撞回 f-1（第一次写就踩过：脚本自己以为有 f-2）
Send $A 'mem:1' 'd3' @{ tool = 'deposit'; args = @{ anchor = $anchor; text = '另一条结论：围栏画在路径上'; epistemic = 'decision' } }
$d3 = WaitCorr $A 'd3'
# 忘掉之前先看一眼账：飞进来的两条都在（**忘掉之后 `alive` 会少**，所以这一眼要早看）
Send $A 'mem:1' 'q0' @{ tool = 'status'; args = @{ anchor = $anchor } }
$q0 = WaitCorr $A 'q0'
Send $A 'mem:1' 'r1' @{ tool = 'recall'; args = @{ anchor = $anchor } }
$r1 = WaitCorr $A 'r1'
Send $A 'mem:1' 'r2' @{ tool = 'recall'; args = @{ anchor = $anchor; if_none_match = $r1.payload.data.etag } }
$r2 = WaitCorr $A 'r2'
Send $A 'mem:1' 'p1' @{ tool = 'pin'; args = @{ anchor = $anchor; id = $d1.payload.data.id } }
$p1 = WaitCorr $A 'p1'
Send $A 'mem:1' 'r3' @{ tool = 'recall'; args = @{ anchor = $anchor } }
$r3 = WaitCorr $A 'r3'
Send $A 'mem:1' 'f1' @{ tool = 'forget'; args = @{ anchor = $anchor; id = $d3.payload.data.id; why = '这条写得不准，作废' } }
$f1 = WaitCorr $A 'f1'
Send $A 'mem:1' 'r4' @{ tool = 'recall'; args = @{ anchor = $anchor } }
$r4 = WaitCorr $A 'r4'
Send $A 'mem:1' 'r5' @{ tool = 'recall'; args = @{ anchor = $anchor; include_forgotten = $true } }
$r5 = WaitCorr $A 'r5'
# 用错法的四次（每一条都要有"报错即手册"的样子）
Send $A 'mem:1' 'e1' @{ tool = 'forget'; args = @{ anchor = $anchor; id = 'f-1'; why = '  ' } }
$e1 = WaitCorr $A 'e1'
Send $A 'mem:1' 'e2' @{ tool = 'deposit'; args = @{ anchor = $anchor; text = ('啊' * 9000) } }
$e2 = WaitCorr $A 'e2'
Send $A 'mem:1' 'e3' @{ tool = 'deposit'; args = @{ anchor = $anchor; text = '分类乱写'; epistemic = '我觉得' } }
$e3 = WaitCorr $A 'e3'
Send $A 'mem:1' 'e4' @{ tool = 'deposit'; args = @{ text = '没说要挂哪' } }
$e4 = WaitCorr $A 'e4'
Send $A 'mem:1' 'e5' @{ tool = 'recall'; args = @{ anchor = $anchor; ttl_ms = -1 } }
$e5 = WaitCorr $A 'e5'
Send $A 'mem:1' 'e6' @{ tool = 'deposit'; args = @{ anchor = $anchor; text = '寿命写错了'; ttl_ms = -1 } }
$e6 = WaitCorr $A 'e6'
# 智能体：只有 mem.read ⇒ 写不了
Send $A 'agent:dada' 't1' @{ task = '把这件事记下来' }
$t1 = WaitCorr $A 't1'
Send $A 'mem:1' 'q1' @{ tool = 'status'; args = @{} }
$q1 = WaitCorr $A 'q1'
$Aerr = Stop-Ctrl $A
$Arecs = Get-Content -LiteralPath (Join-Path (Join-Path (Join-Path $A.dataDir 'sessions') $A.sid) 'log.jsonl') |
    Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json }

$st = $q1.payload.data.totals
Check '1. 服务自报的 manifest **过了注册关**，而且没进过 degraded' {
    $q1.payload.effect -eq 'confirmed' -and $q1.payload.data.anchors.Count -ge 1 -and `
        $Aerr -notlike '*manifest 不合规*' -and $Aerr -notlike '*域锁撞上*'
}
Check '2. `deposit` 写进去了：回的是 id + etag + stored=true' {
    $d1.payload.effect -eq 'confirmed' -and $d1.payload.data.stored -eq $true -and `
        $d1.payload.data.id -eq 'f-1' -and $d1.payload.data.etag -like 'sha256:*'
}
Check '3. **同一条话写两遍会被认出来**（幂等），不是攒出第二条' {
    $d2.payload.data.stored -eq $false -and $d2.payload.data.id -eq $d1.payload.data.id -and `
        $q0.payload.data.totals.alive -eq 2
}
Check '4. `recall` 取得回来，而且**带溯源**（谁写的 + 哪次会话）' {
    $it = $r1.payload.data.items | Where-Object { $_.id -eq 'f-1' } | Select-Object -First 1
    $it -and $it.agent -eq 'console' -and $it.session -eq 's-mem' -and $it.epistemic -eq 'decision'
}
Check '5. **`if_none_match` 那条**：指纹一样就只回"没变"，**不再把内容倒一遍**' {
    $r2.payload.data.unchanged -eq $true -and $r2.payload.data.items.Count -eq 0 -and `
        $r2.payload.data.etag -eq $r1.payload.data.etag
}
Check '6. `pin` 之后它进了**确定性层**（`pinned`），而且锚点指纹**跟着动了**' {
    $p1.payload.effect -eq 'confirmed' -and `
        (@($r3.payload.data.pinned | ForEach-Object { $_.id }) -contains 'f-1') -and `
        $r3.payload.data.etag -ne $r1.payload.data.etag
}
Check '7. **`forget` 是立墓碑，不是删行**：默认看不见，`include_forgotten` 还翻得出来' {
    $f1.payload.effect -eq 'confirmed' -and `
        (@($r4.payload.data.items | ForEach-Object { $_.id }) -notcontains $d3.payload.data.id) -and `
        (@($r5.payload.data.items | ForEach-Object { $_.id }) -contains $d3.payload.data.id)
}
Check '8. 借 `forget` 那条错误钉死"报错即手册"：**不写为什么会被拒**' {
    $e1.payload.effect -eq 'refused' -and $e1.payload.why -like '*为什么*' -and `
        $e1.payload.remedy.Length -gt 0
}
Check '9. 文本超长会被拒，而且**指出去哪儿放长文**（碎片只留结论）' {
    $e2.payload.effect -eq 'refused' -and $e2.payload.remedy -like '*pointer*'
}
Check '10. `epistemic` 不在闭合表里会被拒，并把那五个**列出来**' {
    $e3.payload.effect -eq 'refused' -and $e3.payload.remedy -like '*fact*' -and `
        $e3.payload.remedy -like '*opinion*'
}
Check '11. 没给锚点会被拒 —— **服务不猜"这条记忆挂在哪儿"**' {
    $e4.payload.effect -eq 'refused' -and $e4.payload.why -like '*锚点*'
}
Check '12. `ttl_ms` 写负数不是"不过期"，是写错了：报出来' {
    $e6.payload.effect -eq 'refused' -and $e6.payload.why -like '*-1*' -and `
        $e6.payload.remedy -like '*0*'
}
Check '13. **能力校验在服务侧**：智能体只签了 `mem.read`，写就被服务自己挡下' {
    $refus = @($Arecs | Where-Object { $_.from -eq 'mem:1' -and $_.to -eq 'agent:dada' -and `
        $_.payload.effect -eq 'refused' })
    $refus.Count -ge 1 -and ($refus[0].payload.why -like '*mem.write*')
}
Check '14. 用户那一路（console）不受限：同一个工具，用户能写' {
    $d1.payload.effect -eq 'confirmed'
}

Write-Host '== 场景 B：换一个进程、换一次会话，账还在不在 ==' -ForegroundColor Cyan
$B = Start-Ctrl 's-mem2' 'b' '"mem:1"'
for ($i = 0; $i -lt 20; $i++) {
    Start-Sleep -Milliseconds 150
    Send $B 'ctrl' "ready2-$i" @{ op = 'list' }
    $r = WaitCorr $B "ready2-$i" 800
    if ($r -and (@($r.payload.data.instances | ForEach-Object { $_.id }) -contains 'mem:1')) { break }
}
Send $B 'mem:1' 'r6' @{ tool = 'recall'; args = @{ anchor = $anchor } }
$r6 = WaitCorr $B 'r6'
Send $B 'mem:1' 'l1' @{ tool = 'list'; args = @{} }
$l1 = WaitCorr $B 'l1'

Write-Host '== 场景 C：一处碎片库只许一个进程写 ==' -ForegroundColor Cyan
# 直接起第三个 mem 进程，指着**同一个** store —— 它该进 degraded
$psi3 = New-Object System.Diagnostics.ProcessStartInfo
$psi3.FileName = $mem
$psi3.Arguments = "--instance mem:2 --sid s-mem3 --store `"$store`" --default-ttl-ms 0"
$psi3.RedirectStandardInput = $true
$psi3.RedirectStandardOutput = $true
$psi3.RedirectStandardError = $true
$psi3.UseShellExecute = $false
$psi3.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
$p3 = [System.Diagnostics.Process]::Start($psi3)
$first3 = $p3.StandardOutput.ReadLine()
$hello3 = $first3 | ConvertFrom-Json
$dep = [ordered]@{ id = 'm-x'; ts = 0; type = 'request'; from = 'console'; to = 'mem:2'
                   corr = 'x1'; sid = 's-mem3'; authority = 'user'
                   payload = @{ tool = 'deposit'; args = @{ anchor = $anchor; text = '抢着写' } } }
$p3.StandardInput.WriteLine(($dep | ConvertTo-Json -Compress -Depth 12))
$p3.StandardInput.Flush()
$refused3 = $p3.StandardOutput.ReadLine() | ConvertFrom-Json
$p3.StandardInput.Close()
$p3.WaitForExit(4000) | Out-Null
$Berr = Stop-Ctrl $B

Check '15. **换一个进程 + 换一次会话，账还在**（跨会话持久化）' {
    $r6.payload.effect -eq 'confirmed' -and `
        (@($r6.payload.data.pinned | ForEach-Object { $_.id }) -contains $d1.payload.data.id) -and `
        $r6.payload.data.total -eq 1
}
Check '16. `list` 看得见这个锚点（几条活着、几条钉住、几条忘了）' {
    $a = $l1.payload.data.anchors | Where-Object { $_.anchor -eq $anchor } | Select-Object -First 1
    # f-1 钉住且活着、f-2 活着、f-2 被忘掉之后 alive 只剩 f-1 ⇒ 1 / 1 / 1
    $a -and $a.alive -eq 1 -and $a.pinned -eq 1 -and $a.forgotten -eq 1
}
Check '17. **同一个碎片库上第二个进程进 degraded**（在线、但不接活）' {
    $hello3.payload.state -eq 'degraded' -and $hello3.payload.why -like '*另一个 mem 进程*'
}
Check '18. degraded 的那个拒得明明白白，并指出该去找谁' {
    $refused3.payload.effect -eq 'refused' -and $refused3.payload.why -like '*degraded*' -and `
        $refused3.payload.remedy -like '*另一个 mem 进程*'
}
Check '19. 这一路（含那次**能力拒绝**）没有任何一条结果**违反自己声明的形**' {
    @($Arecs | Where-Object { $_.payload.schema_violation }).Count -eq 0
}

Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    Write-Host '== ctrl stderr ==' -ForegroundColor DarkGray
    foreach ($l in ($Aerr -split "`n")) { if ($l.Trim() -ne '') { Write-Host "  $l" -ForegroundColor DarkGray } }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "碎片库留在：$store"
