# **id 是铸造出来的，不是猜出来的。**
#
#   pwsh -File scripts/smoke-ids.ps1
#
# 四层（会话 / 智能体 / 工具实例 / 工具服务）都有自己的 id，而 id **参与鉴权**：
# 绑定表写的是实例 id、归属写的是 `谁@会话`、路由按 id 找收件人。
# 所以"id 唯一"不是洁癖 —— 撞 id 的后果是**注册表里静默覆盖**：
# 新进程顶掉旧进程，发往那个 id 的消息转手交给它。那是 id 层面的冒充。
#
# 这份脚本钉的就是这一条，外加两个 id 源：
#   1. 服务表里两个人写同一个 id ⇒ **后来者被拒，先来者照常活着**
#   2. `spawn` 出来的 id **只增不减 + 带随机段**（原来用"现有实例数"当后缀，会回退）
#   3. 会话 id 带随机段（原来只有毫秒，两个会话同时开就会写同一个日志目录）
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$echo = Join-Path $root 'target\debug\harness-echo.exe'
function J([string]$p) { return ($p -replace '\\', '\\') }
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
function Start-CtrlSvc([string]$sid, [string]$servicesPath, [string]$dataDir) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $ctrl
    $psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 3000"
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
    return [System.Diagnostics.Process]::Start($psi)
}
function Read-Until([System.Diagnostics.Process]$p, [hashtable]$sink, [scriptblock]$done, [int]$timeoutSec = 15) {
    $t = $null
    $deadline = [DateTime]::UtcNow.AddSeconds($timeoutSec)
    while ([DateTime]::UtcNow -lt $deadline) {
        if ($t -eq $null) { $t = $p.StandardOutput.ReadLineAsync() }
        if (-not $t.Wait(500)) { continue }
        $line = $t.Result
        $t = $null
        if ($null -eq $line -or $line.Trim() -eq '') { continue }
        $m = $line | ConvertFrom-Json
        if ($m.type -eq 'result' -and $m.corr) { $sink[$m.corr] = $m }
        if ($m.type -eq 'presence' -and $m.payload.manifest) { $sink['presence'] = $m }
        if (& $done) { return }
    }
}

Write-Host '== 场景 A：服务表里两个人写同一个 id ==' -ForegroundColor Cyan
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-ids-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $work -Force | Out-Null
$sid = 's-ids'
$dataDir = Join-Path $work 'data'
$servicesPath = Join-Path $work 'services.json'
@"
[
  { "kind": "echo", "id": "echo:1", "role": "tool", "cmd": ["$(J $echo)"] },
  { "kind": "echo", "id": "echo:1", "role": "tool", "cmd": ["$(J $echo)"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
$proc = Start-CtrlSvc $sid $servicesPath $dataDir
$seen = @{}
Read-Until $proc $seen { $seen.ContainsKey('presence') } 20
$req = [ordered]@{ id = 'm-ping'; ts = 0; type = 'request'; from = 'console'; to = 'echo:1'
                   corr = 'c-ping'; sid = $sid; authority = 'user'
                   payload = @{ tool = 'ping'; args = @{} } }
$proc.StandardInput.WriteLine(($req | ConvertTo-Json -Compress -Depth 12))
$proc.StandardInput.Flush()
Read-Until $proc $seen { $seen.ContainsKey('c-ping') } 20
$proc.StandardInput.Close()
$proc.WaitForExit(8000) | Out-Null
$err = $proc.StandardError.ReadToEnd()
$r = $seen['c-ping']

Check '1. 撞 id 的**第二个被拒**，而且中控说清了是谁占着它' {
    $err -like '*已经被占用*' -and $err -like '*同一个 id 在同一时刻只能有一个活的持有者*'
}
Check '2. **先来者没被顶掉**：`echo:1` 照常应答' {
    $r -and $r.payload.effect -eq 'confirmed' -and $r.payload.data.pong -eq $true
}
Check '3. 被拒的那个**没有真的起来**' {
    $seen['presence'].payload.manifest.id -like 'echo-mock*'
}

Write-Host '== 场景 B：`spawn` 出来的 id 只增不减 + 带随机段 ==' -ForegroundColor Cyan
$work2 = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-ids2-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $work2 -Force | Out-Null
$sid2 = 's-ids2'
$dataDir2 = Join-Path $work2 'data'
$services2 = Join-Path $work2 'services.json'
@"
[ { "kind": "echo", "id": "echo:base", "role": "tool", "cmd": ["$(J $echo)"] } ]
"@ | Set-Content -LiteralPath $services2 -Encoding utf8
$proc2 = Start-CtrlSvc $sid2 $services2 $dataDir2
$seen2 = @{}
function Send2($body, [string]$corr) {
    $m = [ordered]@{ id = "m-$corr"; ts = 0; type = 'request'; from = 'console'; to = 'ctrl'
                     corr = $corr; sid = $sid2; authority = 'user'; payload = $body }
    $proc2.StandardInput.WriteLine(($m | ConvertTo-Json -Compress -Depth 12))
    $proc2.StandardInput.Flush()
}
Send2 @{ op = 'spawn'; kind = 'echo'; cmd = @($echo) } 'p1'
Read-Until $proc2 $seen2 { $seen2.ContainsKey('p1') } 20
Send2 @{ op = 'spawn'; kind = 'echo'; cmd = @($echo) } 'p2'
Read-Until $proc2 $seen2 { $seen2.ContainsKey('p2') } 20
$id1 = $seen2['p1'].payload.data.instance_id
$id2 = $seen2['p2'].payload.data.instance_id
$proc2.StandardInput.Close()
$proc2.WaitForExit(8000) | Out-Null
$et2 = $proc2.StandardError.ReadToEndAsync(); $et2.Wait(4000) | Out-Null

Check '4. 两次 spawn 拿到**两个不同的 id**' { $id1 -and $id2 -and $id1 -ne $id2 }
Check '5. 而且它们**带上随机段**（不是会重合的纯计数）' {
    $id1 -match '^echo:\d+-[0-9a-f]{8}$' -and $id2 -match '^echo:\d+-[0-9a-f]{8}$'
}
Check '6. 两次都没撞上服务表里那个 `echo:base`' {
    $id1 -ne 'echo:base' -and $id2 -ne 'echo:base'
}

Write-Host '== 场景 C：会话 id 带随机段（不给 --sid 时的默认值）==' -ForegroundColor Cyan
$sids = @()
foreach ($i in 1..2) {
    $w = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-ids3-$i-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
    New-Item -ItemType Directory -Path $w -Force | Out-Null
    $in = Join-Path $w 'in.txt'
    New-Item -ItemType File -Path $in -Force | Out-Null
    Start-Process -FilePath $ctrl `
        -ArgumentList @('--data-dir', $w, '--drain-ms', '500') `
        -RedirectStandardInput $in `
        -RedirectStandardOutput (Join-Path $w 'out.txt') `
        -RedirectStandardError (Join-Path $w 'err.txt') `
        -NoNewWindow -Wait | Out-Null
    $line = @(Get-Content -LiteralPath (Join-Path $w 'err.txt') | Where-Object { $_ -like '*日志：*' }) |
        Select-Object -First 1
    if ($line -match '([^\\/]+)[\\/]log\.jsonl') { $sids += $Matches[1] }
}
Check '7. 会话 id 的形状是 `s-<毫秒>-<8 位十六进制>`（2026-09-20 从 4 位加长，见 id.rs）' {
    $sids.Count -eq 2 -and ($sids | Where-Object { $_ -notmatch '^s-\d+-[0-9a-f]{8}$' }).Count -eq 0
}
Check '8. 先后开两个会话，**id 不同**（不会写进同一个日志目录）' {
    $sids.Count -eq 2 -and $sids[0] -ne $sids[1]
}

Write-Host ''
Write-Host '== 被拒的原因（stderr 原话）==' -ForegroundColor Cyan
$err -split "`n" | Where-Object { $_ -like '*占用*' } | ForEach-Object { Write-Host "  $($_.Trim())" }
Write-Host ''
Write-Host '== spawn 出来的两个 id ==' -ForegroundColor Cyan
Write-Host "  $id1"
Write-Host "  $id2"
Write-Host ''
Write-Host '== 两次默认会话 id ==' -ForegroundColor Cyan
$sids | ForEach-Object { Write-Host "  $_" }
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "工作目录留在：$work"
