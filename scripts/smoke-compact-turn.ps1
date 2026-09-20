# **压缩在智能体循环里真的触发**（README 阶段 4a 欠的那一条）。
#
#   pwsh -File scripts/smoke-compact-turn.ps1
#
# 之前的状态：压缩内核有、循环里也接了，但**从没真的触发过** ——
# 假对话太短，真实模型那一轮也没到阈值。于是"循环里压缩"这条路径
# **只有代码、没有证据**。
#
# 逼它的办法：把模型窗口调小（`--model-window`），让一个回合里的
# 工具往返把上下文顶上去。然后看三件事：
#   1. 压缩**在回合中间**发生了（`pre_turn` 不是这里，这里必须是回合中）
#   2. 摘要**贴着最后一条 user 之前**（§5.4.3 ④ 的位置契约）
#   3. 压缩之后**回合照样收尾** —— 压缩不该把循环搞崩
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$term = Join-Path $root 'target\debug\harness-term.exe'
$agentBin = Join-Path $root 'target\debug\harness-agent.exe'
function J([string]$p) { return ($p -replace '\\', '\\') }
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-ct-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null
$sid = 's-ct'
$dataDir = Join-Path $work 'data'
# 造几轮"话很多"的对话：每轮两次工具往返，历史就一轮一轮涨上去。
#
# ⚠️ **这里必须用"多轮"，不能用"一个超长回合"** —— 因为切点在**轮次边界**上，
# 而一个回合内部没有任何边界：单个回合再长也压不了（这条限制记在决策册里）。
$lines = New-Object System.Collections.Generic.List[string]
for ($turn = 1; $turn -le 4; $turn++) {
    for ($step = 1; $step -le 2; $step++) {
        $lines.Add('{"step":"tool","calls":[{"to":"term:1","tool":"run","args":{"instance_id":"t-1","cmd":"echo ' + ('第' + $turn + '轮第' + $step + '步：' + ('细节 ' * 10)) + '"}}]}')
    }
    $lines.Add('{"step":"text","text":"第 ' + $turn + ' 轮做完了。"}')
}
Set-Content -LiteralPath (Join-Path $work 'script.ndjson') -Value ($lines -join "`n") -Encoding utf8
$servicesPath = Join-Path $work 'services.json'
@"
[
  { "kind": "term", "id": "term:1", "role": "tool", "wants_fence": true,
    "cmd": ["$(J $term)"] },
  { "kind": "agent", "id": "agent:dada", "role": "agent",
    "bindings": [{"instance": "term:1", "capabilities": ["proc.exec"]}],
    "cmd": ["$(J $agentBin)"],
    "args": ["--persona", "达达",
             "--script", "$(J (Join-Path $work 'script.ndjson'))",
             "--model-window", "900",
             "--max-steps", "8"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
# 先开一个终端（脚本第一步要 run，得先有 t-1）
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ctrl
$psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 10000 --host `"$project`""
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
function Ask([string]$to, [string]$corr, [hashtable]$payload) {
    $msg = [ordered]@{
        id = "m-$corr"; ts = 0; type = 'request'; from = 'console'
        to = $to; corr = $corr; sid = $sid; authority = 'user'; payload = $payload
    }
    $proc.StandardInput.WriteLine(($msg | ConvertTo-Json -Compress -Depth 12))
    $proc.StandardInput.Flush()
    $deadline = [DateTime]::UtcNow.AddSeconds(40)
    while ([DateTime]::UtcNow -lt $deadline) {
        $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
        $line = NextLine $left
        if ($null -eq $line) { continue }
        if ($line.Trim() -eq '') { continue }
        $m = $line | ConvertFrom-Json
        if ($m.type -eq 'result' -and $m.corr -eq $corr) { return $m }
    }
    throw "等 $corr 超时"
}
Start-Sleep -Milliseconds 700
$null = Ask 'term:1' 'c-open' @{ tool = 'open'; args = @{ path = '.'; purpose = '压测上下文' } }
# **分四轮问** —— 每一轮都是一条 user 消息，于是轮次边界出现了，压缩才有地方切
$answers = @()
for ($t = 1; $t -le 4; $t++) {
    $answers += Ask 'agent:dada' "c-t$t" @{ task = "第 $t 件事：跑两步" }
}
$answer = $answers[-1]
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
$comp = $records | Where-Object { $_.payload.kind -eq 'compaction' } | Select-Object -First 1
Write-Host '== 判据 ==' -ForegroundColor Cyan
Check '1. **对话推进中压缩了**（不是靠人手动压的）' { $comp -ne $null }
Check '2. 那次压缩带着切点与摘要（重放要靠它把同一刀切回去）' {
    $comp -and $comp.payload.cut -ne $null -and $comp.payload.summary.Length -gt 0
}
Check '3. 摘要**贴着最后一条 user 之前**（§5.4.3 ④ 的位置契约）' {
    # 这条契约**只有在重建之后才看得出来** —— 日志里记的是"发生过什么"，
    # 而"摘要贴在哪儿"是手术后的形状。所以这里照中控 `replay` 那套做一遍：
    # 取出 model_item 序列 → 按 compaction 的 cut / summary_at 把那一刀切回去。
    $items = New-Object System.Collections.Generic.List[object]
    $applied = $false
    foreach ($r in $records) {
        if ($r.payload.kind -eq 'model_item' -and $r.from -eq 'agent:dada') {
            $items.Add($r.payload.item)
        } elseif ($r.payload.kind -eq 'compaction' -and -not $applied) {
            $cut = [int]$r.payload.cut
            $at = [int]$r.payload.summary_at
            $items.RemoveRange(0, [Math]::Min($cut, $items.Count))
            for ($i = $items.Count - 1; $i -ge 0; $i--) { if ($items[$i].kind -eq 'summary') { $items.RemoveAt($i) } }
            $items.Insert([Math]::Min($at, $items.Count), [pscustomobject]@{ kind = 'summary'; text = $r.payload.summary })
            $applied = $true
        }
    }
    $applied -and $items[$comp.payload.summary_at + 1].kind -eq 'user'
}
Check '4. 压缩之后**后面的轮次照样跑完**（压缩没把循环搞崩）' {
    $answer.payload.effect -eq 'confirmed' -and $answer.payload.text.Length -gt 0 -and $answers.Count -eq 4
}
Check '5. 它报了压掉几轮、窗口换到了第几个' {
    $comp.payload.turns_compacted -ge 1 -and $comp.payload.window_after -ge 2
}
Check '6. 智能体的 trace 里记着压缩次数' {
    $answer.payload.trace.compactions -ge 1
}
Write-Host ''
Write-Host '== 那次压缩 ==' -ForegroundColor Cyan
if ($comp) {
    Write-Host "  窗口 $($comp.payload.window_before) → $($comp.payload.window_after)"
    Write-Host "  $($comp.payload.tokens_before) → $($comp.payload.tokens_after) token，压掉 $($comp.payload.turns_compacted) 轮"
    Write-Host "  摘要放在第 $($comp.payload.summary_at) 项"
}
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "工作目录留在：$work"
