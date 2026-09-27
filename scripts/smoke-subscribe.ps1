# **订阅**：让**独立进程**也能看着这条会话（2026-09-27）。
#
#   pwsh -File scripts/smoke-subscribe.ps1
#
# 在这之前，能看见整条会话的只有"拉起中控的那个进程"（它占着 stdout）——
# 于是一个独立的 GUI 想接进来，**没有门**：它既不是服务表里的服务，也不是中控的子进程。
#
# 这一份要证五件事：
#   1. **订阅口开得起来**，而且地址与口令写在会合文件里（别人找得到）；
#   2. **一个独立进程订阅之后，收到这条会话里路由过的每一条消息**（和 console 那份一样）；
#   3. **它是只读的**：发得出消息，但上限是 `observe` —— 想动手会被权限矩阵挡下；
#   4. **它在图上**（注册成实例，kind=channel，ceiling=observe）；
#   5. **它走了就下线**：断开之后 instance 变 offline、索引里有一笔。
#
# ⚠️ 这份脚本自己就是那个"独立进程"：它**不是中控的子进程**，
# 纯靠会合文件 + 回环 TCP 连上去 —— 这就是"每个客户端只要遵守协议就能接进来"的样子。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
if (-not (Test-Path $ctrl)) { throw "产物不在：$ctrl" }
function J([string]$p) { return ($p -replace '\\', '\\') }
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-sub-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $work -Force | Out-Null
$sid = 's-sub'
$dataDir = Join-Path $work 'data'
$servicesPath = Join-Path $work 'services.json'
Set-Content -LiteralPath $servicesPath -Value '[]' -Encoding utf8
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ctrl
$psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 2000 --tick-ms 0"
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
function Say([string]$corr, [hashtable]$payload, [string]$to = 'ctrl') {
    $m = [ordered]@{ id = "m-$corr"; ts = 0; type = 'request'; from = 'console'; to = $to
                     corr = $corr; sid = $script:sid; authority = 'user'; payload = $payload }
    $proc.StandardInput.WriteLine(($m | ConvertTo-Json -Compress -Depth 12))
    $proc.StandardInput.Flush()
}
function Wait([string]$corr, [int]$timeoutMs = 15000) {
    $deadline = [DateTime]::UtcNow.AddMilliseconds($timeoutMs)
    while ([DateTime]::UtcNow -lt $deadline) {
        $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
        $line = NextLine $left
        if (-not $line) { continue }
        if ($line.Trim() -eq '') { continue }
        $m = $line | ConvertFrom-Json
        if ($m.type -eq 'result' -and $m.corr -eq $corr) { return $m }
    }
    return $null
}
function Ask([string]$corr, [hashtable]$payload) { Say $corr $payload; return Wait $corr }
Start-Sleep -Seconds 2

Write-Host '== ① 订阅口在哪儿：会合文件 ==' -ForegroundColor Cyan
$rvPath = Join-Path (Join-Path $dataDir 'rendezvous') "$sid-console.json"
Check '1. 会合文件写出来了（地址 + 口令 + 传输名，别人照着就能连）' {
    if (-not (Test-Path -LiteralPath $rvPath)) { return $false }
    $rv = Get-Content -LiteralPath $rvPath -Raw | ConvertFrom-Json
    $rv.addr -and $rv.token -and $rv.protocol -eq 'ndjson-tcp-1'
}
$rv = Get-Content -LiteralPath $rvPath -Raw | ConvertFrom-Json

Write-Host '== ②③④ 一个独立进程订阅它 ==' -ForegroundColor Cyan
# **注意：这个 TCP 客户端不是中控的子进程** —— 它只拿着会合文件里的东西。
$tcp = New-Object System.Net.Sockets.TcpClient
$tcp.Connect(($rv.addr -split ':')[0], [int](($rv.addr -split ':')[1]))
$stream = $tcp.GetStream()
$writer = New-Object System.IO.StreamWriter($stream, [System.Text.UTF8Encoding]::new($false))
$writer.AutoFlush = $true
$reader = New-Object System.IO.StreamReader($stream, [System.Text.UTF8Encoding]::new($false))
$writer.WriteLine($rv.token)                 # 握手：先交口令
$ack = $reader.ReadLine()                     # 一问一答：它一定先回一句
Check '2. 握手是一问一答（拿不到 ack 就不敢往下说）' {
    $ack -and $ack -like '*"ok"*'
}
# 订阅：**一条普通的 op**（走信封）—— 名字就是它的实例名
$subReq = [ordered]@{ id = 'm-sub'; ts = 0; type = 'request'; from = 'gui:test'; to = 'ctrl'
                      corr = 'c-sub'; sid = $sid; authority = 'user'; payload = @{ op = 'subscribe' } }
$writer.WriteLine(($subReq | ConvertTo-Json -Compress -Depth 12))
$subAck = $reader.ReadLine() | ConvertFrom-Json
Check '3. 订阅被受理了（回的是它自己的名字那条连接）' {
    $subAck.type -eq 'result' -and $subAck.corr -eq 'c-sub' -and $subAck.payload.data.subscribed -eq $true
}

# **在订阅者那条流上等一条特定回信**：那条流里还混着会话的**转发副本**，
# 所以不能"读下一行就当是它"（我第一次就是这么栽的：抓到的是转发过来的 `list`）。
function SubWait([string]$corr, [int]$timeoutMs = 15000) {
    $deadline = [DateTime]::UtcNow.AddMilliseconds($timeoutMs)
    while ([DateTime]::UtcNow -lt $deadline) {
        $t = $reader.ReadLineAsync()
        if (-not $t.Wait(2000)) { continue }
        $l = $t.Result
        if (-not $l) { return $null }
        if ($l.Trim() -eq '') { continue }
        $m = $l | ConvertFrom-Json
        if ($m.type -eq 'result' -and $m.corr -eq $corr) { return $m }
    }
    return $null
}

# **现在让 console 说一句** —— 订阅者该收到**同样那一条**
Say 'c-list' @{ op = 'list' }
$null = Wait 'c-list'
$seen = @()
$deadline = [DateTime]::UtcNow.AddSeconds(6)
while ([DateTime]::UtcNow -lt $deadline) {
    $t = $reader.ReadLineAsync()
    if (-not $t.Wait(2000)) { continue }
    $l = $t.Result
    if (-not $l) { break }
    $seen += $l
    if (($seen | Where-Object { $_ -like '*c-list*' }).Count -ge 1) { break }
}
Check '4. **它收到了会话里那条消息**（和 console 那份是同一条）' {
    ($seen | Where-Object { $_ -like '*c-list*' }).Count -ge 1
}
Check '5. 它在图上：`list` 里看得见 `gui:test`，而且是 `observe` 上限（只读）' {
    $g = (Ask 'c-list2' @{ op = 'list' }).payload.data
    $me = @($g.instances | Where-Object { $_.id -eq 'gui:test' })
    $me.Count -eq 1 -and $me[0].ceiling -eq 'observe' -and $me[0].kind -eq 'channel'
}

Write-Host '== ③ 只读：它发得出消息，但动不了手 ==' -ForegroundColor Cyan
$spawn = [ordered]@{ id = 'm-sp'; ts = 0; type = 'request'; from = 'gui:test'; to = 'ctrl'
                     corr = 'c-sp'; sid = $sid; authority = 'user'
                     payload = @{ op = 'spawn'; kind = 'echo'; cmd = @('cmd.exe', '/c', 'echo x') } }
$writer.WriteLine(($spawn | ConvertTo-Json -Compress -Depth 12))
$spAck = SubWait 'c-sp'
Check '6. 它想 `spawn`：**拒**（权限矩阵：`spawn` 要 user，而它的上限是 observe）' {
    $spAck.type -eq 'result' -and $spAck.payload.effect -eq 'refused' -and $spAck.payload.why -like '*user*'
}
$alive = Ask 'c-alive' @{ op = 'ping' }
Check '7. 拒了它，**会话照常**（中控还活着）' { $alive.payload.effect -eq 'confirmed' }

Write-Host '== ⑤ 它走了 ==' -ForegroundColor Cyan
$tcp.Close()
Start-Sleep -Milliseconds 800
$g2 = (Ask 'c-list3' @{ op = 'list' }).payload.data
$me2 = @($g2.instances | Where-Object { $_.id -eq 'gui:test' })
Check '8. 断开之后它在图上下线了（没有留一个永远写不出去的连接）' {
    $me2.Count -eq 1 -and $me2[0].state -eq 'offline'
}
$proc.StandardInput.Close()
$proc.WaitForExit(8000) | Out-Null
$err = $proc.StandardError.ReadToEnd()
# ⚠️ **别用 `-like` 去匹配带反引号的话**：`-like` 里反引号是**转义符**，
# 于是我那条 `` '*订阅者 `gui:test` 走了*' `` 永远匹配不上（仓库里那条静态检查
# 正是防这个的 —— 而我自己先踩了一次）。要精确匹配就用 `.Contains`。
Check '9. 账上留了痕：订阅与退订各有一笔（谁在什么时候看了这条会话）' {
    $err.Contains('订阅了这条会话') -and $err.Contains('订阅者') -and $err.Contains('走了')
}
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
