# **会话日志侧的溢出**（规范-消息协议 §5.1）（2026-09-27）。
#
#   pwsh -File scripts/smoke-logspill.ps1
#
# §5.1 那条写着"大 payload 溢出成文件、日志里放引用"，而它**一直是空的**：
# 真正让日志不爆的是**上游**那一条（工具自己先截到 50 000 B）——
# 那只管得住工具结果，管不住别的：终端刷屏、一张截图、一段塞进 `args` 的大文本，
# 都会从日志这儿过。而日志是不变量二（"模型可见即已记录"）的载体，
# 它得**自己**保证不被一条消息撑爆。
#
# 这一份要证五件事：
#   1. **大 payload 在日志里是引用**（正文一个字节都不在那一行里）；
#   2. **正文真的在盘上**（溢出文件在会话目录的 `spill/` 下）；
#   3. **引用是可解的**：`log {resolve:true}` 把它换回正文 —— 不变量二要的正是这个；
#   4. **默认不展开**（`resolve` 不写就是不给正文：这条路是智能体恢复上下文走的，
#      它要的是"我放过什么"，不是把那张 20 KB 的截图原样再吃一遍）；
#   5. **小的照旧不溢出**（防"一刀切"），而且**解不开时如实说**（`resolve_error`）。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$echo = Join-Path $root 'target\debug\harness-echo.exe'
foreach ($b in @($ctrl, $echo)) { if (-not (Test-Path $b)) { throw "产物不在：$b" } }
function J([string]$p) { return ($p -replace '\\', '\\') }
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-logspill-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $work -Force | Out-Null
$sid = 's-logspill'
$dataDir = Join-Path $work 'data'
$servicesPath = Join-Path $work 'services.json'
@"
[
  { "kind": "echo", "id": "echo:1", "role": "tool", "cmd": ["$(J $echo)"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ctrl
# ⚠️ **阈值在这儿显式给小**（`--log-inline 8192`）：它是**部署选择**，
# 而默认那个（64 KiB）是给"大块"准备的 —— 拿它当判据的话，得造 100 KB 的 payload 才验得动。
# 这里显式压到 8 KiB，验的是**机制**（超了就溢出 + 引用可解），不是那个默认值。
$psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 3000 --tick-ms 0 --log-inline 8192"
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
function Ask([string]$corr, [string]$to, [hashtable]$payload, [string]$tool = '') {
    $body = if ($tool) { @{ tool = $tool; args = $payload } } else { $payload }
    $m = [ordered]@{ id = "m-$corr"; ts = 0; type = 'request'; from = 'console'; to = $to
                     corr = $corr; sid = $script:sid; authority = 'user'; payload = $body }
    $proc.StandardInput.WriteLine(($m | ConvertTo-Json -Compress -Depth 12))
    $proc.StandardInput.Flush()
    $deadline = [DateTime]::UtcNow.AddMilliseconds(20000)
    while ([DateTime]::UtcNow -lt $deadline) {
        $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
        $line = NextLine $left
        if (-not $line) { continue }
        if ($line.Trim() -eq '') { continue }
        $r = $line | ConvertFrom-Json
        if ($r.type -eq 'result' -and $r.corr -eq $corr) { return $r }
    }
    throw "等 $corr 超时"
}
Start-Sleep -Seconds 2

Write-Host '== 造一条大的：20 KB 的 payload，里面带一个只有正文才有的记号 ==' -ForegroundColor Cyan
$mark = 'SPILL-MARK-9f3a'
$big = "$mark-" + ('x' * 20000)
$null = Ask 'c-big' 'echo:1' @{ text = $big } 'echo'
$null = Ask 'c-small' 'echo:1' @{ text = '小的一条' } 'echo'
$null = Ask 'c-flush' 'ctrl' @{ op = 'barrier'; reason = '读日志之前' }
# ⚠️ **这两条要在关流之前问**（第一次我把它们写在读日志之后，那时 stdin 已经关了）
$resolved = Ask 'c-resolve' 'ctrl' @{ op = 'log'; instance = 'echo:1'; resolve = $true }
$plain = Ask 'c-plain' 'ctrl' @{ op = 'log'; instance = 'echo:1' }
$proc.StandardInput.Close()
$proc.WaitForExit(8000) | Out-Null
$err = $proc.StandardError.ReadToEnd()
$logPath = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl'
$lines = @(Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' })
$records = @($lines | ForEach-Object { $_ | ConvertFrom-Json })

Check '1. **大 payload 在日志里是引用**（那一行里连正文的记号都没有）' {
    $spilled = @($records | Where-Object { $_.payload.spilled })
    $spilled.Count -ge 1 -and (($lines | Where-Object { $_ -like "*$mark*" }).Count -eq 0)
}
Check '2. **正文真的在盘上**：会话目录 `spill/` 下那个文件在，而且大小对得上' {
    $s = @($records | Where-Object { $_.payload.spilled })[0]
    $f = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) $s.payload.spilled.file
    (Test-Path -LiteralPath $f) -and ((Get-Item -LiteralPath $f).Length -eq $s.payload.spilled.bytes)
}
Check '3. **引用是可解的**：`log {resolve:true}` 把正文换回来（不变量二要的就是这个）' {
    $hit = @($resolved.payload.data.records | Where-Object { $_.payload.args.text -like "*$mark*" })
    $hit.Count -ge 1
}
Check '4. **默认不展开**（这条路是智能体恢复上下文走的：给它正文等于把 20 KB 又吃一遍）' {
    $sp = @($plain.payload.data.records | Where-Object { $_.payload.spilled })
    $withText = @($plain.payload.data.records | Where-Object { $_.payload.args -and $_.payload.args.text -like "*$mark*" })
    $sp.Count -ge 1 -and $withText.Count -eq 0
}
Check '5. **小的照旧不溢出**（防一刀切：这条 payload 就几个字节）' {
    $small = @($records | Where-Object { $_.payload.args.text -eq '小的一条' -and -not $_.payload.spilled })
    $small.Count -ge 1
}
Write-Host ''
Write-Host '== 解不开的时候要**如实说** ==' -ForegroundColor Cyan
$s = @($records | Where-Object { $_.payload.spilled })[0]
$spillFile = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) $s.payload.spilled.file
Remove-Item -LiteralPath $spillFile -Force
# 再起一个中控（同一个 data 目录/会话）来解 —— 文件已经被删了
$psi2 = New-Object System.Diagnostics.ProcessStartInfo
$psi2.FileName = $ctrl
$psi2.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 2000 --tick-ms 0 --log-inline 8192 --resume"
$psi2.RedirectStandardInput = $true
$psi2.RedirectStandardOutput = $true
$psi2.RedirectStandardError = $true
$psi2.UseShellExecute = $false
$psi2.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
$proc2 = [System.Diagnostics.Process]::Start($psi2)
Start-Sleep -Seconds 2
$script:proc2 = $proc2
$m = [ordered]@{ id = 'm-r'; ts = 0; type = 'request'; from = 'console'; to = 'ctrl'
                 corr = 'c-lost'; sid = $sid; authority = 'user'
                 payload = @{ op = 'log'; instance = 'echo:1'; resolve = $true } }
$proc2.StandardInput.WriteLine(($m | ConvertTo-Json -Compress -Depth 12))
$proc2.StandardInput.Flush()
$lost = $null
$deadline = [DateTime]::UtcNow.AddSeconds(15)
while ([DateTime]::UtcNow -lt $deadline -and -not $lost) {
    $t = $proc2.StandardOutput.ReadLineAsync()
    if (-not $t.Wait(5000)) { continue }
    $l = $t.Result
    if (-not $l) { break }
    if ($l.Trim() -eq '') { continue }
    $r = $l | ConvertFrom-Json
    if ($r.type -eq 'result' -and $r.corr -eq 'c-lost') { $lost = $r }
}
Check '6. **溢出文件没了 ⇒ 解不开就报出来**（`resolve_error`）—— 不装作重建得出来' {
    $bad = @($lost.payload.data.records | Where-Object { $_.payload.resolve_error })
    $bad.Count -ge 1
}
$proc2.StandardInput.Close()
$proc2.WaitForExit(8000) | Out-Null
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    Write-Host '== ctrl stderr ==' -ForegroundColor DarkGray
    $err -split "`n" | ForEach-Object { if ($_.Trim()) { Write-Host ('  ' + $_) -ForegroundColor DarkGray } }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host ("工作目录留在：" + $work) -ForegroundColor DarkGray
