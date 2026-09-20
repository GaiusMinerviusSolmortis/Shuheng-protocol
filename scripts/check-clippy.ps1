# **lint 闸门**：警告也算失败（2026-09-20）。
#
#   pwsh -File scripts/check-clippy.ps1
#
# 为什么现在才有它 —— 因为它**抓到过一个真 bug**：
# `domain_lock.rs` 里那句 `for f in fences { … return … }`（循环体第一条就 return，
# 于是只占了第一条围栏）是 clippy 的 `never_loop` 一针指出来的。
# 那个洞让 `[A,B]` 与 `[B,C]` 两个编辑器**同时拿到锁**、
# 同时管着同一条路径 —— 而它在这个仓库里躺了**一天多没被任何测试发现**
# （`cargo test` 不会跑 lint，验收脚本里也没有一条用过两个 `--fence`）。
#
# 结论：**测试绿不等于代码干净。** 这条闸门就是那个差的角。
# 代价是每次改完要顺手把警告清掉 —— 而清警告通常顺手就改掉了真东西
# （这次一口气清掉的 11 条里，就有一条"入参 8 个"的函数、一处
#   `let` 绑了个 unit、一处可以合并进守卫的判断）。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== clippy：全部目标，警告即失败 ==' -ForegroundColor Cyan
$out = & cargo clippy --manifest-path (Join-Path $root 'Cargo.toml') --all-targets -- -D warnings 2>&1
$code = $LASTEXITCODE
$bad = @($out | Where-Object { $_ -match '^(warning|error)(\[|:)' })
if ($code -eq 0 -and $bad.Count -eq 0) {
    Write-Host '  [pass] 一条警告都没有。' -ForegroundColor Green
    exit 0
}
Write-Host "  [FAIL] clippy 报了 $($bad.Count) 条（退出码 $code）：" -ForegroundColor Red
$bad | Select-Object -First 40 | ForEach-Object { Write-Host "  $_" -ForegroundColor Red }
Write-Host ''
Write-Host '看不清哪来的就看全量输出：' -ForegroundColor Yellow
Write-Host '  cargo clippy --all-targets' -ForegroundColor Yellow
exit 1
