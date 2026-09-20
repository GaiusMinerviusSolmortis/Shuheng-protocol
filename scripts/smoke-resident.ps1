# **常驻服务**（`lifecycle: resident`）与**第二个传输**的验收。
#
#   pwsh -File scripts/smoke-resident.ps1
#
# 这一块要证的是**分布式那句话到底成不成立**。在这之前：
#   · 每个服务都是中控的子进程，通道是 stdin/stdout —— **父子私有**；
#   · 中控一收摊，那条通道没了，服务跟着走；
#   · 于是"多个会话共享一个长期服务"**根本写不出来**，
#     所谓"多进程组成的图"其实是**每会话一张小星形图**。
#
# 现在要证的：
#
#   会话一 ──拉起──▶ mem:1（写会合文件、听回环端口）
#      │                    ▲
#      └── 收摊 ✕ 不收它     │ **接上（不是再起一个）**
#                       会话二
#
# 判据围着三个问题转：
#   1. **它真活过会话一了吗？**（不是"又起了一个像的"）
#   2. **会话二接上的是同一个进程吗？**（`started_ms` 一样、`sessions_served` 在涨）
#   3. **它真能被收掉吗？**（接上来的那个中控手里没有句柄 ⇒ 走它声明的 `stop` 入口）
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$mem = Join-Path $root 'target\debug\harness-mem.exe'
foreach ($b in @($ctrl, $mem)) { if (-not (Test-Path $b)) { throw "产物不在：$b" } }
function J([string]$p) { return ($p -replace '\\', '\\') }
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-resident-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
# **会合目录是跨会话稳定的**（跟着 work 走，而不是跟着某个 data-dir 走）——
# 这一点是 `resident` 的前提：新会话换个 data 目录也得找得到老进程。
$rendezvous = Join-Path $work 'rendezvous'
$store = Join-Path $work 'memory'
New-Item -ItemType Directory -Path $rendezvous -Force | Out-Null
$anchor = Join-Path $work 'project'
New-Item -ItemType Directory -Path $anchor -Force | Out-Null

function Write-Services([string]$path) {
    @"
[
  { "kind": "mem", "id": "mem:1", "role": "tool",
    "resident": true,
    "rendezvous": "$(J $rendezvous)",
    "cmd": ["$(J $mem)"],
    "args": ["--store", "$(J $store)", "--default-ttl-ms", "0"] }
]
"@ | Set-Content -LiteralPath $path -Encoding utf8
}

function Start-Ctrl([string]$sid, [string]$tag) {
    $dataDir = Join-Path $work "data-$tag"
    $servicesPath = Join-Path $work "services-$tag.json"
    Write-Services $servicesPath
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $ctrl
    $psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 5000 --tick-ms 0"
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
    $proc = [System.Diagnostics.Process]::Start($psi)
    return [pscustomobject]@{ proc = $proc; sid = $sid; dataDir = $dataDir; outTask = $null }
}
function NextLineOf($h, [int]$timeoutMs) {
    if ($null -eq $h.outTask) { $h.outTask = $h.proc.StandardOutput.ReadLineAsync() }
    if (-not $h.outTask.Wait($timeoutMs)) { return $null }
    $line = $h.outTask.Result
    $h.outTask = $null
    return $line
}
$script:seq = 0
function Send($h, [string]$to, [string]$corr, [hashtable]$payload) {
    $script:seq++
    $m = [ordered]@{ id = "m-$($script:seq)"; ts = 0; type = 'request'; from = 'console'
                     to = $to; corr = $corr; sid = $h.sid; authority = 'user'; payload = $payload }
    $h.proc.StandardInput.WriteLine(($m | ConvertTo-Json -Compress -Depth 12))
    $h.proc.StandardInput.Flush()
}
function WaitCorr($h, [string]$corr, [int]$timeoutMs = 25000) {
    $deadline = [DateTime]::UtcNow.AddMilliseconds($timeoutMs)
    while ([DateTime]::UtcNow -lt $deadline) {
        $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
        $line = NextLineOf $h $left
        if ($null -eq $line) { continue }
        if ($line.Trim() -eq '') { continue }
        $m = $line | ConvertFrom-Json
        if ($m.type -eq 'result' -and $m.corr -eq $corr) { return $m }
    }
    return $null
}
function Wait-Mem($h, [int]$tries = 40) {
    for ($i = 0; $i -lt $tries; $i++) {
        Start-Sleep -Milliseconds 150
        Send $h 'ctrl' "ready-$($h.sid)-$i" @{ op = 'list' }
        $r = WaitCorr $h "ready-$($h.sid)-$i" 800
        if ($r -and (@($r.payload.data.instances | ForEach-Object { $_.id }) -contains 'mem:1')) { return $r }
    }
    return $null
}
function Stop-Ctrl($h) {
    $h.proc.StandardInput.Close()
    $h.proc.WaitForExit(10000) | Out-Null
    try { $d = $h.proc.StandardOutput.ReadToEndAsync(); $d.Wait(2000) | Out-Null } catch { }
    $e = $h.proc.StandardError.ReadToEndAsync(); $e.Wait(4000) | Out-Null
    if ($e.IsCompleted) { return $e.Result } else { return '' }
}

Write-Host '== 会话一：拉起常驻服务，写一条记忆，然后收摊 ==' -ForegroundColor Cyan
$A = Start-Ctrl 's-res-a' 'a'
$null = Wait-Mem $A
Send $A 'mem:1' 'd1' @{ tool = 'deposit'; args = @{ anchor = $anchor; text = '跨会话的第一条记忆'; epistemic = 'decision' } }
$d1 = WaitCorr $A 'd1'
Send $A 'mem:1' 'st1' @{ tool = 'status'; args = @{} }
$st1 = WaitCorr $A 'st1'
$Aerr = Stop-Ctrl $A

$rvPath = Join-Path $rendezvous 'mem_1.json'
$rv1 = if (Test-Path $rvPath) { Get-Content -LiteralPath $rvPath -Raw | ConvertFrom-Json } else { $null }
$pidAlive = $false
if ($rv1) { $pidAlive = $null -ne (Get-Process -Id $rv1.pid -ErrorAction SilentlyContinue) }

Write-Host '== 会话二：接上同一个进程（不是再起一个） ==' -ForegroundColor Cyan
$B = Start-Ctrl 's-res-b' 'b'
$null = Wait-Mem $B
Send $B 'mem:1' 'r1' @{ tool = 'recall'; args = @{ anchor = $anchor } }
$r1 = WaitCorr $B 'r1'
Send $B 'mem:1' 'st2' @{ tool = 'status'; args = @{} }
$st2 = WaitCorr $B 'st2'
$rv2 = Get-Content -LiteralPath $rvPath -Raw | ConvertFrom-Json
$Berr = Stop-Ctrl $B
# ⚠️ **这一眼必须在会话三之前看**：判据是最后一起算的，而会话三会把它收掉 ——
#    在最后那个时刻再去 `Get-Process`，看到的永远是"没了"。
#    （"什么时候采样"是判据本身的一部分，这是这一轮第二次踩它。）
$pidAliveAfterB = $null -ne (Get-Process -Id $rv1.pid -ErrorAction SilentlyContinue)

Write-Host '== 会话三：把它收掉（接上来的人手里没有句柄） ==' -ForegroundColor Cyan
$C = Start-Ctrl 's-res-c' 'c'
$null = Wait-Mem $C
Send $C 'ctrl' 'stop1' @{ op = 'stop'; instance = 'mem:1' }
$stop1 = WaitCorr $C 'stop1' 30000
# 它自己停要一点时间（撤会合文件、退出）
$gone = $false
for ($i = 0; $i -lt 30; $i++) {
    Start-Sleep -Milliseconds 200
    $alive = $null -ne (Get-Process -Id $rv1.pid -ErrorAction SilentlyContinue)
    if (-not $alive) { $gone = $true; break }
}
$Cerr = Stop-Ctrl $C

Write-Host '== 判据 ==' -ForegroundColor Cyan
Check '1. 常驻服务自报的 manifest 过了闸门（`resident` 不再被拦）' {
    $d1.payload.effect -eq 'confirmed' -and $Aerr -notlike '*manifest 不合规*' -and `
        $Aerr -notlike '*声明了 `lifecycle: resident`*'
}
Check '2. 第一次是**拉起**，而且它写下了会合文件（地址 + 口令 + pid）' {
    $Aerr -like '*拉起常驻 mem:1*' -and $rv1 -and $rv1.addr -like '127.0.0.1:*' -and `
        $rv1.token.Length -eq 64 -and $rv1.protocol -eq 'ndjson-tcp-1'
}
Check '3. **会话一收摊之后，它还在跑**（进程还活着）' {
    $Aerr -like '*这个会话收摊不动它*' -and $pidAlive
}
Check '4. 会话二**接上**的是同一个进程，不是又起了一个' {
    # ⚠️ 中控那行是 `**接上** mem:1`（带加粗标记）—— 第一版判据把 `**` 忘了，
    #    于是"证据明明在 stderr 里"却判成失败。判据和被测文本，两边都得看。
    $Berr -like '*接上*mem:1*' -and $Berr -notlike '*拉起常驻*mem:1*' -and `
        $rv2.started_ms -eq $rv1.started_ms -and $rv2.pid -eq $rv1.pid
}
Check '5. 会话二**读得到会话一写的记忆**（跨会话这件事真的成立）' {
    $r1.payload.effect -eq 'confirmed' -and `
        (@($r1.payload.data.items | ForEach-Object { $_.text }) -contains '跨会话的第一条记忆')
}
Check '6. 服务自己数得清它服务过几个会话（`sessions_served` 在涨）' {
    # **它的日志在会合目录里**（不继承中控的 stderr）—— 那本来就是"想找它的人会去看"的地方
    $svcLog = Join-Path $rendezvous 'mem_1.log'
    (Test-Path $svcLog) -and ((Get-Content -LiteralPath $svcLog -Raw) -like '*会话 1 接上了*')
}
Check '7. 会话二收摊之后，它**还是**没死' {
    $Berr -like '*这个会话收摊不动它*' -and `
        $pidAliveAfterB
}
Check '8. 接上来的会话调 `stop`：**中控明说它没有句柄**，走的是"请它自己停"' {
    $stop1.payload.effect -eq 'confirmed' -and $stop1.payload.data.asked_to_stop -eq $true -and `
        ($stop1.payload.data.how -like '*别的会话起来的*' -or $stop1.payload.data.how -like '*它自己停的*')
}
Check '9. 它**真的停了**（进程没了）—— 走的是它自己声明的 `stop` 入口，不是我杀它' {
    $gone -and $Cerr -notlike '*没收掉*'
}
Check '10. 它走的时候把会合文件撤了（不然下一个会话会以为它还在）' {
    -not (Test-Path $rvPath) -or ((Get-Content -LiteralPath $rvPath -Raw | ConvertFrom-Json).pid -ne $rv1.pid)
}

Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    Write-Host '== 会话一 stderr ==' -ForegroundColor DarkGray
    foreach ($l in ($Aerr -split "`n")) { if ($l.Trim() -ne '') { Write-Host "  $l" -ForegroundColor DarkGray } }
    Write-Host '== 会话二 stderr ==' -ForegroundColor DarkGray
    foreach ($l in ($Berr -split "`n")) { if ($l.Trim() -ne '') { Write-Host "  $l" -ForegroundColor DarkGray } }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "工作目录留在：$work"
