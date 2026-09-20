# 阶段 3 的验收：**线性记忆内核**。
#
#   pwsh -File scripts/smoke-memory.ps1
#
# 它验的是压缩这件事的四条硬要求（03-上下文 §5.4.3 / §5.4.4）：
#   1. 不到触发线绝不压
#   2. 压完要落到目标附近，而不是刚好压回去一点（"反正都是炸缓存，不如放新东西少点"）
#   3. 切点在轮次边界 —— **一次工具调用和它的结果绝不能被切开**
#   4. 压缩是一等事件：新窗口、留了旧窗口 id、pre/post 钩子都记着
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$bin = Join-Path $root 'target\debug\harness-memory.exe'
if (-not (Test-Path $bin)) { throw "产物不在：$bin" }
function Run([string[]]$extra) {
    $out = & $bin demo --json @extra 2>&1
    if ($LASTEXITCODE -ne 0) { throw "demo 跑失败：$out" }
    return ($out | ConvertFrom-Json)
}
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
Write-Host '== 场景 A：窗口 1200 / 触发 80% / 压到 40% / 留 2 轮 ==' -ForegroundColor Cyan
$a = Run @('--window', '1200', '--ratio', '0.8', '--aggressiveness', '0.5', '--keep', '2', '--turns', '16')
$trigger = [int]$a.policy.trigger_tokens
$target = [int]$a.policy.target_tokens
$fired = @($a.steps | Where-Object { $_.triggered })
$c = $a.compactions | Select-Object -First 1
Check '1. 只有到触发线才压，而且只压一次' {
    $fired.Count -eq 1 -and $a.compactions.Count -eq 1
}
Check '2. 触发前每一轮都在线以下' {
    $before = @($a.steps | Where-Object { $_.turn -lt $fired[0].turn })
    ($before | Where-Object { $_.tokens -ge $trigger }).Count -eq 0 -and $before.Count -ge 5
}
Check "3. 触发那一轮确实越过触发线（$($fired[0].tokens) ≥ $trigger）" {
    $fired[0].tokens -ge $trigger
}
Check '4. 压完落到目标附近，不是只压回一点点' {
    $c.tokens_after -le $target -and $c.tokens_after -lt $c.tokens_before
}
Check '5. 窗口换了一个，并且留着上一个的 id（可回溯）' {
    $c.window_before -eq 1 -and $c.window_after -eq 2 -and $c.window_id_after -like 'w2-*' -and `
        $c.window_id_before -like 'w1-*' -and $a.final_window -eq 2
}
Check '6. 理由与时机都记着' {
    $c.reason -eq 'context_limit' -and $c.when -eq 'pre_turn' -and $c.strategy -eq 'linear/memento' -and `
        $c.implementation -eq 'mock' -and $c.status -eq 'done'
}
Check '7. 压掉的原文留在记录里（"原文去哪了"有答案）' {
    $c.removed.Count -gt 0 -and $c.summary_bytes -gt 0
}
Check '8. 摘要放在系统前缀后面（PreTurn 的位置契约）' {
    $a.summary_at -eq 1
}
Check '9. 切点是安全的：没有半对的工具调用' {
    $a.safe -eq $true
}
Check '10. 压完之后每一轮都还在窗口里（没有被压成空的）' {
    $after = @($a.steps | Where-Object { $_.turn -gt $fired[0].turn })
    $after.Count -ge 3 -and ($after | Where-Object { $_.tokens -le 0 }).Count -eq 0
}
Write-Host ''
Write-Host '== 场景 B：压得更狠（aggressiveness 0.9）该压掉更多 ==' -ForegroundColor Cyan
$b = Run @('--window', '1200', '--ratio', '0.8', '--aggressiveness', '0.9', '--keep', '1', '--turns', '16')
$cb = $b.compactions | Select-Object -First 1
Check '11. 更狠的配置压掉更多轮、剩下更小' {
    $cb.turns_compacted -ge $c.turns_compacted -and $cb.tokens_after -le $c.tokens_after
}
Check '12. 压完的目标比例跟着走（0.9 ⇒ 目标 = 8%）' {
    # 注意：**目标可能达不到** —— "至少保留 keep_recent_turns 轮"是硬约束，
    # 压不过头。所以这里断言的是"目标算对了"+"确实压到了触发线以下"。
    [int]$b.policy.target_tokens -eq 96 -and $cb.tokens_after -le [int]$b.policy.trigger_tokens
}
Write-Host ''
Write-Host '== 场景 C：窗口很大 ⇒ 一次都不该压 ==' -ForegroundColor Cyan
$z = Run @('--window', '200000', '--ratio', '0.8', '--aggressiveness', '0.5', '--keep', '2', '--turns', '16')
Check '13. 装得下就不动历史，也不换窗口' {
    $z.compactions.Count -eq 0 -and $z.final_window -eq 1 -and $z.summary_at -eq $null -and `
        ($z.steps | Where-Object { $_.triggered }).Count -eq 0
}
Write-Host ''
Write-Host '== 场景 D：摘要器坏掉（只给 20 字符）时不许崩 ==' -ForegroundColor Cyan
$d = Run @('--window', '900', '--ratio', '0.8', '--aggressiveness', '0.5', '--keep', '1', '--turns', '18', '--summary-chars', '20')
Check '14. 摘要再小也照样保持配对安全，而且会再压一次' {
    $d.safe -eq $true -and $d.compactions.Count -ge 1 -and $d.final_window -ge 2
}
Write-Host ''
Write-Host '== 场景 A 的轨迹 ==' -ForegroundColor Cyan
$a.steps | ForEach-Object {
    $mark = if ($_.triggered) { '  ← 触发压缩' } else { '' }
    Write-Host ("  轮 {0,2}  {1,6} token  窗口 {2}{3}" -f $_.turn, $_.tokens, $_.window, $mark)
}
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
