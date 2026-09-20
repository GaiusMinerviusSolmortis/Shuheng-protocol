# **决策册的计数校验。**
#
#   pwsh -File scripts/check-decisions.ps1
#
# 为什么要有它：`10-决策.md` 里每个 `### 小节（N 条）` 的 N 都是**手写的**，
# 而条目是**后加的** —— 一边加一边忘了改数字，正是这份文档最容易烂的方式。
# 2026-09-19 第一次跑它就抓到一条（`#28` 已经作废了还赖在 §4 里，
# 而那个小节的计数是按"它不算"写的）。
#
# 规则（和小节标题里那个 N 的含义一致）：
#   §2 / §3 / §5 —— 条目是行首的 `- `（`§4` 例外，见下）
#   §4 待定     —— 条目是行首的 `数字. `
$ErrorActionPreference = 'Stop'
$path = Join-Path (Split-Path -Parent $PSScriptRoot) '10-决策.md'
$lines = Get-Content -LiteralPath $path
$entries = New-Object System.Collections.Generic.List[object]
$state = $null
$cur = $null
$declared = 0
$count = 0
function Flush {
    if ($null -ne $script:cur) {
        $script:entries.Add([pscustomobject]@{ name = $script:cur; declared = $script:declared; counted = $script:count })
    }
    $script:cur = $null
}
foreach ($line in $lines) {
    if ($line -match '^## 3\.') { Flush; $state = 'dash'; continue }
    if ($line -match '^## 4\.') { Flush; $state = 'num';  continue }
    if ($line -match '^## 5\.') { Flush; $state = 'dash'; continue }
    if ($line -match '^## ')     { Flush; $state = $null;  continue }
    if ($line -match '^### (.+?)（(\d+) 条）\s*$') {
        Flush
        $cur = $Matches[1]
        $declared = [int]$Matches[2]
        $count = 0
        continue
    }
    if ($null -eq $cur) { continue }
    if ($state -eq 'dash' -and $line -match '^- ') { $count++ }
    if ($state -eq 'num'  -and $line -match '^\d+\. ') { $count++ }
}
Flush
$bad = 0
foreach ($e in $entries) {
    if ($e.declared -eq $e.counted) {
        Write-Host ("  [ok]   {0}：{1} 条" -f $e.name, $e.counted) -ForegroundColor DarkGray
    } else {
        Write-Host ("  [FAIL] {0}：标题写 {1} 条，实际 {2} 条" -f $e.name, $e.declared, $e.counted) -ForegroundColor Red
        $bad++
    }
}
if ($bad -gt 0) {
    Write-Host "决策册计数对不上：$bad 个小节。" -ForegroundColor Red
    exit 1
}
Write-Host ("决策册计数一致（{0} 个小节）。" -f $entries.Count) -ForegroundColor Green
