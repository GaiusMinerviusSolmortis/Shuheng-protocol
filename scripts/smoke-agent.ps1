# 阶段 4a 的验收：**智能体实例**。
#
#   pwsh -File scripts/smoke-agent.ps1
#
# 这是第一次**真的闭上一个环**：
#   console ──task──▶ ctrl ──▶ agent ──模型（脚本）──▶ ctrl ──▶ 工具实例
#                        ▲                                    │
#                        └──────────── result ────────────────┘
#         而且 agent 会把上一次的结果**读进去再决定下一步**（不是写死 id）。
#
# 两个场景：
#   A 正常干完 + 中途**工具崩了**（敕令真的发出来，未决调用被结掉）
#   B 调错了工具 → 收到"报错即手册" → 改对继续
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$term = Join-Path $root 'target\debug\harness-term.exe'
$echo = Join-Path $root 'target\debug\harness-echo.exe'
$agentBin = Join-Path $root 'target\debug\harness-agent.exe'
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
foreach ($b in @($ctrl, $term, $echo, $agentBin)) { if (-not (Test-Path $b)) { throw "产物不在：$b" } }
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
function J([string]$p) { return ($p -replace '\\', '\\') }
function Run-Scenario([string]$name, [string]$scriptBody, [string]$task) {
    $work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-agent-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
    $project = Join-Path $work 'project'
    New-Item -ItemType Directory -Path $project -Force | Out-Null
    Set-Content -LiteralPath (Join-Path $project 'hello.txt') -Value 'hi' -Encoding utf8
    $sid = "s-$name"
    $dataDir = Join-Path $work 'data'
    $scriptPath = Join-Path $work 'script.ndjson'
    Set-Content -LiteralPath $scriptPath -Value $scriptBody -Encoding utf8
    # （这里曾经写一份 `cards.json`：**没人读它**。名片只有服务自己的 manifest
    #   一个来源（§11.1）—— 2026-09-19 对着代码查差距时删掉。）
    $servicesPath = Join-Path $work 'services.json'
    @"
[
  { "kind": "term", "id": "term:1", "role": "tool",
    "cmd": ["$(J $term)"], "args": ["--fence", "$(J $project)"] },
  { "kind": "echo", "id": "echo:1", "role": "tool", "cmd": ["$(J $echo)"] },
  { "kind": "agent", "id": "agent:dada", "role": "agent", "bindings": ["term:1", "echo:1"],
    "cmd": ["$(J $agentBin)"],
    "args": ["--persona", "达达", "--persona-version", "1",
             "--fence", "$(J $project)",
              "--script", "$(J $scriptPath)",
             "--model-window", "8000"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $ctrl
    # ⚠️ **托管是必须的**（2026-09-27）：不托管路径的会话不能使唤带围栏的服务。
    # 这一份本来就在一个项目里干活 —— 把那条路径**托出来**，才是它真实的形状。
    $psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 8000 --host `"$project`""
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
    $proc = [System.Diagnostics.Process]::Start($psi)
    $script:outTask = $null
    $script:all = New-Object System.Collections.Generic.List[string]
    $script:replies = @{}
    function NextLine([int]$timeoutMs) {
        if ($null -eq $script:outTask) { $script:outTask = $proc.StandardOutput.ReadLineAsync() }
        if (-not $script:outTask.Wait($timeoutMs)) { return $null }
        $line = $script:outTask.Result
        $script:outTask = $null
        return $line
    }
    $msg = [ordered]@{
        id = 'm-task-1'; ts = 0; type = 'request'; from = 'console'
        to = 'agent:dada'; corr = 'c-task'; sid = $sid; authority = 'user'
        payload = @{ task = $task }
    }
    $proc.StandardInput.WriteLine(($msg | ConvertTo-Json -Compress -Depth 12))
    $proc.StandardInput.Flush()
    $deadline = [DateTime]::UtcNow.AddSeconds(40)
    while ([DateTime]::UtcNow -lt $deadline) {
        $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
        $line = NextLine $left
        if ($null -eq $line) { continue }
        if ($line.Trim() -eq '') { continue }
        $script:all.Add($line)
        $m = $line | ConvertFrom-Json
        if ($m.type -eq 'result') { $script:replies[$m.corr] = $m }
        if ($m.type -eq 'result' -and $m.corr -eq 'c-task') { break }
    }
    $proc.StandardInput.Close()
    $proc.WaitForExit(8000) | Out-Null
    # 收尾的排空要**容忍手上还挂着一个没读完的 ReadLineAsync** ——
    # 同一根流上再挂一个新的会直接抛 "stream is currently in use"
    try {
        $drain = $proc.StandardOutput.ReadToEndAsync(); $drain.Wait(4000) | Out-Null
        if ($drain.IsCompleted -and $drain.Result) { foreach ($l in ($drain.Result -split "`n")) { if ($l.Trim() -ne '') { $script:all.Add($l.Trim()) } } }
    } catch { }
    $et = $proc.StandardError.ReadToEndAsync(); $et.Wait(4000) | Out-Null
    $logPath = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl'
    $records = @()
    if (Test-Path $logPath) { $records = @(Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json }) }
    return [pscustomobject]@{
        work = $work; project = $project; sid = $sid
        msgs = $script:all; replies = $script:replies; log = $records
        stderr = $(if ($et.IsCompleted) { $et.Result } else { '' })
    }
}
Write-Host '== 场景 A：干完活，中途工具崩了 ==' -ForegroundColor Cyan
$scriptA = @'
{"step":"tool","calls":[{"to":"term:1","tool":"open","args":{"path":".","purpose":"跑测试"}}]}
{"step":"tool","calls":[{"to":"term:1","tool":"run","args":{"instance_id":"$last.instance_id","cmd":"echo AGENT-STEP-1"}}]}
{"step":"tool","calls":[{"to":"echo:1","tool":"boom","args":{}}]}
{"step":"text","text":"测试跑完了；另外 echo 工具崩了，我不再指望它。"}
'@
$A = Run-Scenario 'A' $scriptA '把项目里的测试跑一遍'
$ra = $A.replies['c-task']
Check '1. 智能体把结论回给了发起方' {
    $ra -and $ra.from -eq 'agent:dada' -and $ra.to -eq 'console' -and $ra.payload.effect -eq 'confirmed' -and $ra.payload.text.Length -gt 0
}
Check '2. 它真的跑了 4 步、调了 3 次工具' {
    $ra.payload.trace.steps -eq 4 -and $ra.payload.trace.tool_calls -eq 3
}
Check '3. **它读了上一步的结果再决定下一步**（用的是 open 返回的实例 id）' {
    $run = $A.log | Where-Object { $_.type -eq 'request' -and $_.from -eq 'agent:dada' -and $_.payload.tool -eq 'run' }
    $run -and $run.payload.args.instance_id -eq 't-1'
}
Check '4. 命令真在终端里执行了' {
    $res = $A.log | Where-Object { $_.type -eq 'result' -and $_.from -eq 'term:1' -and $_.payload.output -like '*AGENT-STEP-1*' }
    $res -ne $null
}
Check '5. **敕令真的发出去了**：工具崩了之后中控往智能体注了一条' {
    $inj = $A.log | Where-Object { $_.type -eq 'inject' -and $_.from -eq 'ctrl' -and $_.to -eq 'agent:dada' }
    $inj -and $inj.payload.text -like '*echo:1*下线*' -and $inj.payload.reason -eq 'service_down'
}
Check '6. 智能体收到了那条敕令（进上下文，但不改前缀）' {
    $ra.payload.trace.injects -eq 1
}
Check '7. 崩掉的那次调用**没被静默丢弃**：中控给了 unverifiable' {
    $r = $A.log | Where-Object { $_.type -eq 'result' -and $_.from -eq 'ctrl' -and $_.payload.code -eq 'service_down' -and $_.to -eq 'agent:dada' }
    $r -and $r.payload.effect -eq 'unverifiable'
}
Check '7b. **中控自己算出了"能不能重试"**（不再把判断推回给模型）' {
    # `boom` 声明的是 destructive + 不幂等 ⇒ 该给"别自动重试"
    $r = $A.log | Where-Object { $_.type -eq 'result' -and $_.payload.code -eq 'service_down' -and $_.to -eq 'agent:dada' } | Select-Object -First 1
    $r -and $r.payload.remedy -like '*别自动重试*' -and $r.payload.remedy -like '*先看状态*'
}
Check '8. **不变量二**：模型看到的那段系统提示进了会话日志' {
    $ev = $A.log | Where-Object { $_.type -eq 'event' -and $_.payload.kind -eq 'prompt_compiled' }
    $ev -and $ev.payload.system -like '*达达*' -and $ev.payload.system -like '*term:1*' -and $ev.payload.manifest.compiler
}
Check '9. 组合清单留了痕（用了哪些部件、哪个版本）' {
    $ev = $A.log | Where-Object { $_.payload.kind -eq 'prompt_compiled' } | Select-Object -First 1
    $ev.payload.manifest.persona -eq '达达' -and $ev.payload.manifest.persona_version -eq '1' -and `
        ($ev.payload.manifest.tool_instances -contains 'term:1')
}
Check '10. 提示词里说清了"名片不是手册"和围栏不是故障' {
    $sys = ($A.log | Where-Object { $_.payload.kind -eq 'prompt_compiled' } | Select-Object -First 1).payload.system
    $sys -like '*不是完整参数表*' -and $sys -like '*越界会被挡下来*'
}
Write-Host ''
Write-Host '== 场景 B：先调错，再读着报错改对 ==' -ForegroundColor Cyan
$scriptB = @'
{"step":"tool","calls":[{"to":"term:9","tool":"run","args":{"cmd":"echo x"}}]}
{"step":"tool","calls":[{"to":"term:1","tool":"open","args":{"path":".","purpose":"纠错之后重来"}}]}
{"step":"text","text":"刚才调错了一个不存在的工具，读着报错换成 term:1 已经开好了。"}
'@
$B = Run-Scenario 'B' $scriptB '帮我看看项目'
$rb = $B.replies['c-task']
Check '11. 调错之后它还能把活干完' {
    $rb -and $rb.payload.effect -eq 'confirmed' -and $rb.payload.trace.tool_calls -eq 2 -and $rb.payload.text.Length -gt 0
}
Check '12. 报错是**发给智能体的、带出口的**（instance_gone + remedy）' {
    # **它得是一次 `result`**（规范 §3：一次 request 以恰好一个 result 收尾）。
    # 原来中控把这类拒绝放在 `event` 里发 —— 智能体那边只好加一条 `event` 分支
    # 去接它，否则会一直等一个永远不来的结果。协议那头修好之后，这里也钉紧了。
    $f = $B.log | Where-Object { $_.type -eq 'result' -and $_.to -eq 'agent:dada' -and $_.payload.code -eq 'instance_gone' }
    $f -and $f.payload.why -like '*term:9*' -and $f.payload.remedy
}
Check '13. 第二次调用用的是对的实例，而且真的开成了' {
    $ok = $B.log | Where-Object { $_.type -eq 'result' -and $_.from -eq 'term:1' -and $_.payload.instance_id }
    $ok -ne $null
}
Write-Host ''
Write-Host '== 场景 C：模型吐了畸形 JSON（参数根本解不出来）==' -ForegroundColor Cyan
# 这一格演的是真 provider 那儿的形状：`arguments` **是一个字符串**，
# 而那个字符串不是合法 JSON。第一版的做法会编一个 `{_raw,_parse_error}` 发出去 ——
# 那等于**替模型编了它没说的话**，而工具会回一句"缺少参数"，指不到真正的原因。
$scriptC = @'
{"step":"tool","calls":[{"to":"term:1","tool":"open","args_raw":"{path: \".\", purpose: 半截"}]}
{"step":"tool","calls":[{"to":"term:1","tool":"open","args":{"path":".","purpose":"读了手册之后改对的"}}]}
{"step":"text","text":"第一版参数写坏了，读了报错里的参数表改对，开好了。"}
'@
$C = Run-Scenario 'C' $scriptC '把项目打开'
$rc = $C.replies['c-task']
Check '14. **那一条调用根本没发出去**：工具那边一条请求都没收到' {
    $toTerm = @($C.log | Where-Object { $_.type -eq 'request' -and $_.from -eq 'agent:dada' -and $_.to -eq 'term:1' })
    # 只该有**第二次**那一条（改对之后的）；坏的那条一次都不许发
    $toTerm.Count -eq 1 -and $toTerm[0].payload.args.purpose -eq '读了手册之后改对的'
}
Check '15. 它被**记成一次坏调用**，而不是一次工具调用（两种失败要分得开）' {
    $rc.payload.trace.bad_calls -eq 1 -and $rc.payload.trace.tool_calls -eq 1
}
Check '16. **报错即手册**：回给模型的那条里有原话、有"这次没发生"、有**抄来的参数表**' {
    $item = $C.log | Where-Object { $_.payload.kind -eq 'model_item' -and `
        $_.payload.item.kind -eq 'tool_result' -and $_.payload.item.text -like '*参数不合法*' } | Select-Object -First 1
    # ⚠️ 判据里**别在 `-like` 的模式里写反引号**：`-like` 走的是通配符引擎，
    # 那里反引号是**转义符**（跟"单引号字符串里当字面量"不是一回事）——
    # 于是 `*…`help`*` 里最后那个 `*` 会被吃成**字面星号**，模式就永远匹配不上。
    # （这条和上一轮"`**接上**` 忘了算加粗标记"是同一个病：判据和被测文本，两边都得看。）
    $item -and $item.payload.item.text -like '*没有发生*' -and `
        $item.payload.item.text -like '*"path"*' -and `
        $item.payload.item.text -like '*先调它的*help*'
}
Check '17. 而且它**能救回来**：读着那条手册改对之后，活干完了' {
    $rc.payload.effect -eq 'confirmed' -and `
        ($C.log | Where-Object { $_.type -eq 'result' -and $_.from -eq 'term:1' -and $_.payload.instance_id }) -ne $null
}
Write-Host ''
Write-Host '== 场景 A 的消息流（截断）==' -ForegroundColor Cyan
$A.msgs | ForEach-Object {
    $m = $_ | ConvertFrom-Json
    $code = if ($m.payload.code) { $m.payload.code } else { '' }
    Write-Host ("{0,-9} {1,-8} {2,-11} -> {3,-11} {4}" -f $m.corr, $m.type, $m.from, $m.to, $code)
}
Write-Host ''
Write-Host '== 场景 A 的会话日志（这就是"可重放"的依据）==' -ForegroundColor Cyan
$A.log | ForEach-Object {
    $k = if ($_.payload.kind) { $_.payload.kind } elseif ($_.payload.tool) { $_.payload.tool } else { '' }
    Write-Host ("{0,3} {1,-8} {2,-11} -> {3,-11} {4}" -f $_.seq, $_.type, $_.from, $_.to, $k)
}
Write-Host ''
Write-Host '== 场景 B 的消息流 ==' -ForegroundColor Cyan
$B.msgs | ForEach-Object {
    $m = $_ | ConvertFrom-Json
    $code = if ($m.payload.code) { $m.payload.code } else { '' }
    Write-Host ("{0,-9} {1,-8} {2,-11} -> {3,-11} {4}" -f $m.corr, $m.type, $m.from, $m.to, $code)
}
Write-Host "场景 B 工作目录：$($B.work)"
if ($failures.Count -gt 0) {
    Write-Host ''
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$($A.work)"
    exit 1
}
Write-Host ''
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "场景 A 工作目录留在：$($A.work)"
