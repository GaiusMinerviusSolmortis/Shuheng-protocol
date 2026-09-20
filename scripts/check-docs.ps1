# **文档里的数字要对得上账**（2026-09-20）。
#
#   pwsh -File scripts/check-docs.ps1
#
# 为什么要有它：`check-decisions.ps1` 只管**决策册的小节计数**，
# 而 README 里还散着一堆手写的数字 —— 2026-09-20 的审计一口气抓到四处：
#
#   ① 「怎么跑」里写着 `cargo test # 160 个测试`，真值 228；
#   ② 文档债里写着「06-操控.md 仍有 1205 行」，真值 850；
#   ③ **8 个脚本**的「N 条判据」过期（实跑 362 条，文档写 343 条）；
#   ④ 同一份 README 里 `smoke-stdio` 一边写 10 条、一边写 11 条。
#
# 形状和"两份真相一定会漂"那条一模一样：**能数出来的别写死；
# 写死了，就得有人查。** 这个脚本就是那个"有人查"。
#
# 两条纪律：
#   · 判据条数按**脚本里 `Check` 的调用点**数 —— 今天它与实跑条数逐条相等
#     （32 个脚本、362 条都核过）。要是以后有人把 `Check` 塞进循环里，
#     这里会少算，**那就得改成另一种数法**（别装作它还对）。
#   · 测试条数是**跑出来的**（`cargo test` 的汇总），不是数 `#[test]`
#     —— 属性会因为 `cfg` 掉队，数出来不等于跑到的。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
function Say([string]$s) { Write-Host "         $s" -ForegroundColor DarkGray }

$readme = Get-Content (Join-Path $root 'README.md')

Write-Host '== 脚本清单：README 说的数 vs 磁盘上的数 ==' -ForegroundColor Cyan
$smokeOnDisk = @(Get-ChildItem (Join-Path $root 'scripts') -Filter 'smoke-*.ps1').Count
$checkOnDisk = @(Get-ChildItem (Join-Path $root 'scripts') -Filter 'check-*.ps1').Count
$crateOnDisk = @(Get-ChildItem (Join-Path $root 'crates') -Directory).Count
$claimed = $null
foreach ($l in $readme) {
    if ($l -match '\*\*(\d+)\*\*\s*个 crate.*?\*\*(\d+)\*\*\s*个 smoke 脚本.*?\*\*(\d+)\*\*\s*个 check 脚本') {
        $claimed = @([int]$matches[1], [int]$matches[2], [int]$matches[3])
    }
}
if ($null -eq $claimed) { throw 'README 里没找到「N 个 crate + N 个 smoke 脚本 + N 个 check 脚本」那句话' }
Check "crate 数（README $($claimed[0])）" { $claimed[0] -eq $crateOnDisk }
Check "smoke 脚本数（README $($claimed[1])）" { $claimed[1] -eq $smokeOnDisk }
Check "check 脚本数（README $($claimed[2])）" { $claimed[2] -eq $checkOnDisk }
Say "磁盘上：$crateOnDisk 个 crate / $smokeOnDisk 个 smoke / $checkOnDisk 个 check"

Write-Host ''
Write-Host '== 每个脚本的「N 条判据」 ==' -ForegroundColor Cyan
$stale = @()
$totalClaimed = 0
$totalActual = 0
$claimed = @{}
foreach ($l in $readme) {
    if ($l -match 'scripts/([a-z0-9\-]+\.ps1)\s+#\s*(.*?)(\d+)\s*条判据') {
        $file = $matches[1]
        $said = [int]$matches[3]
        $path = Join-Path $root "scripts/$file"
        if (-not (Test-Path $path)) { $stale += "$file：README 里有它，磁盘上没有"; continue }
        $claimed[$file] = $true
        $actual = ([regex]::Matches((Get-Content $path -Raw), '(?m)^\s*Check\s')).Count
        $totalClaimed += $said
        $totalActual += $actual
        if ($said -ne $actual) { $stale += "$file：README 写 $said，实际 $actual" }
    }
}
# **每个 smoke 脚本都得在上面那一栏里**：新写的脚本忘了登记，
# 就会悄悄绕开这道查账（"不许漏"比"漏了也没事"重要）。
foreach ($f in (Get-ChildItem (Join-Path $root 'scripts') -Filter 'smoke-*.ps1').Name) {
    if (-not $claimed.ContainsKey($f)) { $stale += "$f：**没在 README 的清单里**（新脚本要登记一行）" }
}
Check "$($claimed.Count) 个脚本的条数逐个对得上（合计 $totalActual 条）" { $stale.Count -eq 0 }
foreach ($s in $stale) { Say $s }

Write-Host ''
Write-Host '== 测试总数（跑出来的）==' -ForegroundColor Cyan
Write-Host '  正在跑 cargo test ...' -ForegroundColor DarkGray
$out = & cargo test --manifest-path (Join-Path $root 'Cargo.toml') --no-fail-fast 2>&1
if ($LASTEXITCODE -ne 0) { $failures.Add('cargo test 自己没过') | Out-Null }
$measured = 0
foreach ($line in $out) {
    if ($line -match 'test result: ok\. (\d+) passed') { $measured += [int]$matches[1] }
}
$saidTests = @()
foreach ($l in $readme) {
    foreach ($m in [regex]::Matches($l, '(\d+) 个测试')) { $saidTests += [int]$m.Groups[1].Value }
}
Check "README 说的测试数与实跑一致（实跑 $measured）" {
    $saidTests.Count -gt 0 -and ($saidTests | Where-Object { $_ -ne $measured }).Count -eq 0
}
Say ("README 里写的：" + (($saidTests | Sort-Object -Unique) -join ' / ') + "；实跑：" + $measured)

Write-Host ''
Write-Host '== 文档行数 ==' -ForegroundColor Cyan
$lineClaims = @()
foreach ($l in $readme) {
    # ⚠️ 文件名里带汉字（`06-操控.md`）—— 别用 `[0-9\-]+` 那种只认 ASCII 的写法，
    # 那样它**一条都匹配不到，然后安安静静地通过**（第一版就踩了这个）。
    if ($l -match '([^\s*`]+\.md)\s*仍有\s*(\d+)\s*行') { $lineClaims += @($matches[1], [int]$matches[2]) }
}
Check 'README 里那条"某个 .md 有多少行"确实被读到了' { $lineClaims.Count -ge 2 }
if ($lineClaims.Count -ge 2) {
    $file = $lineClaims[0]
    $said = $lineClaims[1]
    $actual = (Get-Content (Join-Path $root $file) | Measure-Object -Line).Lines
    Check "$file 的行数（README 说 $said）" { $said -eq $actual }
    Say "实际：$actual 行"
}

Write-Host ''
Write-Host '== 决策册的汇总那一行（模块地图里） ==' -ForegroundColor Cyan
# ⚠️ 这一格**被漏过两次**（写 256、真值 257；后来又一次）。
# `check-decisions.ps1` 只查**每个小节**的计数，汇总那一行一直是"没人查"的。
$m = $null
foreach ($l in $readme) {
    if ($l -match '已定\s*\*\*(\d+)\*\*\s*\+\s*待定\s*\*\*(\d+)\*\*\s*\+\s*已解决待清账\s*(\d+)') {
        $m = @([int]$matches[1], [int]$matches[2], [int]$matches[3])
    }
}
Check 'README 那一行读得到' { $null -ne $m }
if ($null -ne $m) {
    $decision = Get-Content (Join-Path $root '10-决策.md')
    $zone = ''; $sum = @{ settled = 0; open = 0 }
    foreach ($l in $decision) {
        if ($l -match '^## 3\.') { $zone = 'settled'; continue }
        if ($l -match '^## 4\.') { $zone = 'open'; continue }
        if ($l -match '^## 5\.') { $zone = 'done'; continue }
        if ($l -match '^###\s.*（(\d+)\s*条）' -and $zone -ne 'done') {
            if ($sum.ContainsKey($zone)) { $sum[$zone] += [int]$matches[1] }
        }
    }
    $doneMatch = $decision | Where-Object { $_ -match '^## 5\..*（(\d+)\s*条）' } | Select-Object -First 1
    $doneCount = if ($doneMatch -and $doneMatch -match '（(\d+)\s*条）') { [int]$matches[1] } else { -1 }
    Check "已定（README $($m[0])，小节合计 $($sum.settled)）" { $m[0] -eq $sum.settled }
    Check "待定（README $($m[1])，小节合计 $($sum.open)）" { $m[1] -eq $sum.open }
    Check "已解决待清账（README $($m[2])，正文 $doneCount）" { $m[2] -eq $doneCount }
}

Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "对不上账 $($failures.Count) 处：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host '**要么改文档，要么改代码 —— 但别让两边各说各的。**' -ForegroundColor Yellow
    exit 1
}
Write-Host '文档里的数字都对得上账。' -ForegroundColor Green
