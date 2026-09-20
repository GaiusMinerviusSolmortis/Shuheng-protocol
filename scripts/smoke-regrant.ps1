# **运行期改签：授权可以追加，也可以收回。**
#
#   pwsh -File scripts/smoke-regrant.ps1
#
# 这一轮补的是那条一直缺的**触发源**。在这之前，绑定只是服务表里的一行静态配置，
# 于是"名册只追加 + 回合边界扫一遍 + 发一条敕令"整套机制**永远等不到变化** ——
# 链是接好的，就是没人按开关。
#
# 现在按下它，整条链自己走完（每一条都在这份脚本里有判据）：
#
#   用户 grant ──▶ 中控改签 ──▶ **敕令**（事实/影响/出口）
#                                   │
#                                   ▼
#        智能体下一个回合开始前扫一遍 ──▶ **换名册**（function 定义数组）
#                                   │
#        系统提示**一个字都不动**（§15.2：拼装冻结，变化走 append-only）
#
# 场景 A 用一个 mock 脚本演三次**一模一样**的调用：第一次被授权挡下、
# 第二次成功、第三次又被挡下 —— 差别只在授权，这就是要证的东西。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$term = Join-Path $root 'target\debug\harness-term.exe'
$echo = Join-Path $root 'target\debug\harness-echo.exe'
$agentBin = Join-Path $root 'target\debug\harness-agent.exe'
function J([string]$p) { return ($p -replace '\\', '\\') }
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}

# ---------------------------------------------------------------- 场景 A
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-regrant-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null
$sid = 's-regrant'
$dataDir = Join-Path $work 'data'
$scriptPath = Join-Path $work 'script.ndjson'
# 三幕，每幕都是"先调 echo:1，再给结论" —— **调的是同一个工具，不同的是授权**
@'
{"step":"tool","calls":[{"to":"echo:1","tool":"ping","args":{}}]}
{"step":"text","text":"第一幕：echo 我还用不了。"}
{"step":"tool","calls":[{"to":"echo:1","tool":"ping","args":{}}]}
{"step":"text","text":"第二幕：echo 现在能用了。"}
{"step":"tool","calls":[{"to":"echo:1","tool":"ping","args":{}}]}
{"step":"text","text":"第三幕：echo 又被收回去了。"}
'@ | Set-Content -LiteralPath $scriptPath -Encoding utf8
$servicesPath = Join-Path $work 'services.json'
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
$psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 8000"
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
function Send($body, [string]$corr, [string]$to = 'ctrl') {
    $m = [ordered]@{ id = "m-$corr"; ts = 0; type = 'request'; from = 'console'; to = $to
                     corr = $corr; sid = $sid; authority = 'user'; payload = $body }
    $proc.StandardInput.WriteLine(($m | ConvertTo-Json -Compress -Depth 12))
    $proc.StandardInput.Flush()
}
function Wait-Corr([string]$corr, [int]$timeoutSec = 45) {
    $deadline = [DateTime]::UtcNow.AddSeconds($timeoutSec)
    while ([DateTime]::UtcNow -lt $deadline) {
        $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
        $line = NextLine $left
        if ($null -eq $line) { continue }
        if ($line.Trim() -eq '') { continue }
        $m = $line | ConvertFrom-Json
        if ($m.type -eq 'result' -and $m.corr -eq $corr) { return $m }
    }
    return $null
}

Write-Host '== 场景 A：grant → 敕令 → 换名册 → revoke ==' -ForegroundColor Cyan
# 顺带一条**控制台直连工具**的诊断调用：它是"用户那一侧"的样本，
# 后面场景 B 里同一个工具、同一条 `status` 会被拒 —— 两个放一起看才有意义
Send @{ tool = 'status'; args = @{} } 'c-diag' 'term:1'
Send @{ task = '试着用 echo' } 't1' 'agent:dada'
$r1 = Wait-Corr 't1'
Send @{ op = 'grant'; agent = 'agent:dada'; instance = 'echo:1' } 'g1'
$g1 = Wait-Corr 'g1'
Send @{ task = '再用一次 echo' } 't2' 'agent:dada'
$r2 = Wait-Corr 't2'
Send @{ op = 'revoke'; agent = 'agent:dada'; instance = 'echo:1' } 'r1'
$rv = Wait-Corr 'r1'
Send @{ task = '第三次用 echo' } 't3' 'agent:dada'
$r3 = Wait-Corr 't3'
$proc.StandardInput.Close()
$proc.WaitForExit(10000) | Out-Null
$et = $proc.StandardError.ReadToEndAsync(); $et.Wait(4000) | Out-Null
$logPath = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl'
$records = @(Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json })

$rejected = @($records | Where-Object { $_.type -eq 'result' -and $_.to -eq 'agent:dada' -and $_.payload.code -eq 'refused' -and $_.payload.why -like '*没有使用*echo:1*' })
$echoOk = @($records | Where-Object { $_.type -eq 'result' -and $_.from -eq 'echo:1' -and $_.payload.effect -eq 'confirmed' })
$injGrant = @($records | Where-Object { $_.type -eq 'inject' -and $_.payload.reason -eq 'binding_granted' })
$injRevoke = @($records | Where-Object { $_.type -eq 'inject' -and $_.payload.reason -eq 'binding_revoked' })
$changes = @($records | Where-Object { $_.payload.kind -eq 'toolset_changed' })
$promptEvents = @($records | Where-Object { $_.payload.kind -eq 'prompt_compiled' })
$injected = @($records | Where-Object { $_.payload.kind -eq 'model_item' -and $_.payload.item.kind -eq 'injected' })

Check '1. 第一幕：**没授权就够不着** —— 中控挡下了那次 echo 调用' { $rejected.Count -ge 2 }
Check '2. `grant` 生效了，回复里带着改签之后的整张清单' {
    $g1 -and $g1.payload.effect -eq 'confirmed' -and ($g1.payload.data.bindings -contains 'echo:1') -and `
        ($g1.payload.data.bindings -contains 'term:1')
}
Check '3. **敕令立刻发出去了**（不是等下一次装配）' {
    $injGrant.Count -ge 1 -and $injGrant[0].to -eq 'agent:dada' -and $injGrant[0].payload.instance -eq 'echo:1'
}
Check '4. 敕令是**三段式**的（事实 / 影响 / 出口）' {
    $t = $injGrant[0].payload.text
    $t -like '*事实*' -and $t -like '*影响*' -and $t -like '*出口*' -and $t -like '*可以用*'
}
Check '5. 敕令真的**进了模型的上下文**（不只是躺在日志里）' {
    ($injected | Where-Object { $_.payload.item.text -like '*可以用*echo:1*' }).Count -ge 1
}
Check '6. 第二幕：**同一个调用现在成了** —— 名册换过之后它够得着了' {
    $echoOk.Count -ge 1 -and $r2.payload.effect -eq 'confirmed'
}
Check '7. **换名册这件事本身也进了日志**（不变量二：模型可见即已记录）' {
    $changes.Count -ge 2 -and ($changes[0].payload.after -contains 'echo:1')
}
Check '8. **系统提示没有被重编** —— 前缀冻结，变化只走敕令（§15.2）' {
    $promptEvents.Count -eq 1
}
Check '9. `revoke` 之后清单里没有它了' {
    $rv -and $rv.payload.effect -eq 'confirmed' -and ($rv.payload.data.bindings -notcontains 'echo:1') -and `
        ($rv.payload.data.bindings -contains 'term:1')
}
Check '10. 收回也有敕令，而且说清"已经发出去的调用不受影响"' {
    $injRevoke.Count -ge 1 -and $injRevoke[0].payload.text -like '*不再能用*' -and `
        $injRevoke[0].payload.text -like '*追不回来*'
}
Check '11. 第三幕：又被挡下了，而且拒绝里说清**它被授权的是什么**' {
    ($rejected | Where-Object { $_.corr -ne $null }).Count -ge 2 -and `
        ($rejected.payload.remedy -join ' ') -like '*你被授权的是*term:1*'
}
Check '12. 三次任务都收掉了（授权变动没把回合打断）' {
    $r1.payload.effect -eq 'confirmed' -and $r3.payload.effect -eq 'confirmed'
}

# ---------------------------------------------------------------- 场景 B
Write-Host '== 场景 B：智能体那条通道想给自己加权限 ==' -ForegroundColor Cyan
$work2 = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-regrant2-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$proj2 = Join-Path $work2 'project'
New-Item -ItemType Directory -Path $proj2 -Force | Out-Null
$sid2 = 's-regrant2'
$dataDir2 = Join-Path $work2 'data'
$script2 = Join-Path $work2 'script.ndjson'
Set-Content -LiteralPath $script2 -Value '{"step":"text","text":"我在。"}' -Encoding utf8
$services2 = Join-Path $work2 'services.json'
@"
[
  { "kind": "term",  "id": "term:1",  "role": "tool",
    "cmd": ["$(J $term)"], "args": ["--fence", "$(J $proj2)"] },
  { "kind": "echo",  "id": "echo:1",  "role": "tool", "cmd": ["$(J $echo)"] },
  { "kind": "agent", "id": "agent:dada", "role": "agent", "bindings": [],
    "cmd": ["$(J $agentBin)"],
    "args": ["--persona", "达达", "--fence", "$(J $proj2)", "--script", "$(J $script2)"] }
]
"@ | Set-Content -LiteralPath $services2 -Encoding utf8
$psi2 = New-Object System.Diagnostics.ProcessStartInfo
$psi2.FileName = $ctrl
# **这条口子的权威上限只有 agent** —— 它转发来的消息都到不了 user
$psi2.Arguments = "--sid $sid2 --data-dir `"$dataDir2`" --services `"$services2`" --drain-ms 5000 --channel 微信:agent"
$psi2.RedirectStandardInput = $true
$psi2.RedirectStandardOutput = $true
$psi2.RedirectStandardError = $true
$psi2.UseShellExecute = $false
$psi2.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
$proc2 = [System.Diagnostics.Process]::Start($psi2)
$script:outTask2 = $null
function NextLine2([int]$timeoutMs) {
    if ($null -eq $script:outTask2) { $script:outTask2 = $proc2.StandardOutput.ReadLineAsync() }
    if (-not $script:outTask2.Wait($timeoutMs)) { return $null }
    $line = $script:outTask2.Result
    $script:outTask2 = $null
    return $line
}
# 注意 from 必须是那条通道的 id（通道只能替自己说话）
function Send2($body, [string]$corr, [string]$to) {
    $m = [ordered]@{ id = "m-$corr"; ts = 0; type = 'request'; from = '微信'; to = $to
                     corr = $corr; sid = $sid2; authority = 'user'; payload = $body }
    $proc2.StandardInput.WriteLine(($m | ConvertTo-Json -Compress -Depth 12))
    $proc2.StandardInput.Flush()
}
$replies2 = @{}
# 等它把服务装配起来（诊断那两条要打到 term:1 上）
$ready = $false
for ($i = 0; $i -lt 20 -and -not $ready; $i++) {
    $corr = "c2-ready-$i"
    Send2 @{ op = 'list' } $corr 'ctrl'
    $deadline = [DateTime]::UtcNow.AddMilliseconds(500)
    while ([DateTime]::UtcNow -lt $deadline -and -not $replies2.ContainsKey($corr)) {
        $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
        $line = NextLine2 $left
        if ($null -eq $line) { continue }
        if ($line.Trim() -eq '') { continue }
        $m = $line | ConvertFrom-Json
        if ($m.type -eq 'result' -and $m.corr) { $replies2[$m.corr] = $m }
    }
    if ($replies2.ContainsKey($corr)) {
        $ids = @($replies2[$corr].payload.data.instances | ForEach-Object { $_.id })
        $ready = ($ids -contains 'term:1') -and ($ids -contains 'agent:dada')
    }
    if (-not $ready) { Start-Sleep -Milliseconds 200 }
}
Send2 @{ op = 'grant'; agent = 'agent:dada'; instance = 'echo:1' } 'g2' 'ctrl'
Send2 @{ tool = 'run'; args = @{ instance_id = 't-1'; cmd = 'echo hi' } } 'g3' 'term:1'
Send2 @{ tool = 'status'; args = @{} } 'g4' 'term:1'
$deadline = [DateTime]::UtcNow.AddSeconds(25)
while ([DateTime]::UtcNow -lt $deadline -and -not ($replies2.ContainsKey('g2') -and `
        $replies2.ContainsKey('g3') -and $replies2.ContainsKey('g4'))) {
    $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
    $line = NextLine2 $left
    if ($null -eq $line) { continue }
    if ($line.Trim() -eq '') { continue }
    $m = $line | ConvertFrom-Json
    if ($m.type -eq 'result' -and $m.corr) { $replies2[$m.corr] = $m }
}
$g2 = $replies2['g2']
$g3 = $replies2['g3']
$g4 = $replies2['g4']
$proc2.StandardInput.Close()
$proc2.WaitForExit(8000) | Out-Null
$et2 = $proc2.StandardError.ReadToEndAsync(); $et2.Wait(4000) | Out-Null

Check '13. **智能体那条通道加不了权限**：被降级到 agent 之后就发不出去了' {
    $g2 -and $g2.payload.effect -eq 'refused' -and $g2.payload.why -like '*user*级别*'
}
Check '14. 拒绝里点明"智能体给自己加权限是不允许的"' {
    $g2.payload.remedy -like '*不允许*'
}
Check '15. **被降级的通道指挥不动工具**：`proc.exec` 拿不到（这条补的是个真洞）' {
    $g3 -and $g3.payload.effect -eq 'refused' -and $g3.payload.why -like '*proc.exec*' -and `
        $g3.payload.caller_kind -eq 'channel'
}
Check '16. 但它**还能问话**：零能力的 `status` 照常放行（诊断通道不能跟着一起坏）' {
    $g4 -and $g4.payload.effect -eq 'confirmed'
}

# 「盖章」这件事本身也得看得见 —— 否则上面那些判据只是"结果对了"，说不清为什么对
$consoleReq = @($records | Where-Object { $_.type -eq 'request' -and $_.from -eq 'console' -and $_.to -eq 'term:1' })
$agentReqs = @($records | Where-Object { $_.type -eq 'request' -and $_.from -eq 'agent:dada' -and $_.to -eq 'echo:1' })
Check '17. 控制台直连工具的请求被盖成 `user` + 全权（它跟场景 B 那条 `status` 是同一个工具）' {
    $consoleReq.Count -ge 1 -and $consoleReq[0].payload.caller_kind -eq 'user' -and `
        ($consoleReq[0].payload.granted -contains '*')
}
Check '18. 智能体的请求被盖成 `agent`，而且**授权前空表、授权后拿到全权**' {
    $agentReqs.Count -ge 3 -and $agentReqs[0].payload.caller_kind -eq 'agent' -and `
        $agentReqs[0].payload.granted.Count -eq 0 -and ($agentReqs[1].payload.granted -contains '*')
}

Write-Host ''
Write-Host '== 第一条敕令（grant）==' -ForegroundColor Cyan
if ($injGrant.Count -gt 0) { $injGrant[0].payload.text -split "`n" | ForEach-Object { Write-Host "  $_" } }
Write-Host ''
Write-Host '== 第二条敕令（revoke）==' -ForegroundColor Cyan
if ($injRevoke.Count -gt 0) { $injRevoke[0].payload.text -split "`n" | ForEach-Object { Write-Host "  $_" } }
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "工作目录留在：$work"
