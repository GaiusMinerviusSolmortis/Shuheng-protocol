# **层与层的界限：谁该认识什么、谁不该认识什么**（2026-09-23 起）。
#
#   pwsh -File scripts/check-layers.ps1
#
# 为什么要有这一份：这套系统的分界线**不是画在纸上就成立的**，它是"某一份源码里
# 有没有那个词"的事。而这类界限一旦被悄悄越回去，**运行时判据一条都不会红** ——
# 功能照样对，只是又多了一层耦合。
#
# 现在只有一条（L1）：
#
#   **中控不许认识"上下文"的结构。**
#
# 判据蠢得很直接：`harness-ctrl` 的源码里**不许出现**那三个记录标识符
# （所以那段注释里也故意不把它们写出来 —— 写出来这条判据就查不出来了）。
# 谁把"记录 → 上下文"的重建挪回中控，或者在中控里顺手解析一下模型条目，这三个词就会回来。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
function Hits([string]$dir, [string[]]$words) {
    $out = @()
    foreach ($w in $words) {
        $hits = @(Get-ChildItem $dir -Recurse -Filter *.rs | Select-String -Pattern $w -SimpleMatch)
        foreach ($h in $hits) { $out += "$($h.Filename):$($h.LineNumber)  $w" }
    }
    return $out
}
$ctxWords = @('prompt_compiled', 'model_item', 'compaction')
$ctrlHits = Hits (Join-Path $root 'crates/harness-ctrl/src') $ctxWords
Check '1. **中控不认识上下文的结构**：`harness-ctrl` 里没有那三个记录标识符' {
    $ctrlHits.Count -eq 0
}
foreach ($h in $ctrlHits) { Write-Host "         $h" -ForegroundColor DarkGray }
# **防判据空转**：要是两边都没有那三个词（比如有人把记录名改了），上面那条会**假通过**。
# 所以反过来钉一句：它们必须在**智能体**那边（重建归它）。
$agentHits = Hits (Join-Path $root 'crates/harness-agent/src') $ctxWords
Check '2. 而它们**确实在智能体那边**（重建归它；两边都没有 = 上一条在空转）' {
    (@($agentHits | Where-Object { $_ -like '*prompt_compiled*' }).Count -ge 1) -and
    (@($agentHits | Where-Object { $_ -like '*model_item*' }).Count -ge 1) -and
    (@($agentHits | Where-Object { $_ -like '*compaction*' }).Count -ge 1)
}
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}
Write-Host '层的界限是对的。' -ForegroundColor Green
