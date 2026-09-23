# **中控的 op 名册**：权限矩阵 + 两个一直没有判据的 op（2026-09-23）。
#
#   pwsh -File scripts/smoke-ctrl-ops.ps1
#
# 背景：规范 §2.7 那张名册里有 **20 个 op / 19 条分支**，而在这之前
# **只有 2 个查过 authority**（`prompt` 和 `grant|revoke`）—— 其余 17 个一律不查。
# 于是任何一个"能往中控发 request"的实例（**包括工具服务**，它们的权威上限是 `observe`）
# 都能 `spawn` 任意可执行文件、`stop` 别人的服务、`shutdown` 整个中控。
#
# 这一份要证四件事：
#   1. **只读那档真的只读**（`ping` / `list` / `index` / `session` / `prompts`）；
#   2. **会改状态或寿命的那些，低级别一律拒** —— 而且**真的没动手**
#      （不是"回了一句拒绝"就算：`spawn` 那条要看它有没有偷偷把进程拉起来）；
#   3. **`replay` 只能查自己**（它是 `agent` 档，可它能读到整场会话的上下文 ——
#      包括系统提示和全部用户消息）；
#   4. **user 档照常**：矩阵不许把正常路堵上（`spawn`/`stop` 照样能用）。
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
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-ops-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $work -Force | Out-Null
$sid = 's-ops'
$dataDir = Join-Path $work 'data'
$servicesPath = Join-Path $work 'services.json'
@"
[
  { "kind": "echo", "id": "echo:1", "role": "tool", "cmd": ["$(J $echo)"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ctrl
$psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 3000 --tick-ms 0"
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
# 中控进程放在**全局**里：下面要起第二个中控（另一条上限的入口），
# 而 Send/Op 这些辅助函数得跟着换 —— 写死一个局部变量就换不动了。
$script:proc = [System.Diagnostics.Process]::Start($psi)
$proc = $script:proc
$script:outTask = $null
$script:seq = 0
# **发件人必须是这条通道自己**（中控把 `from` 钉在通道 id 上：`on_channel_line` 那条
# "别在控制通道上冒充别的实例"）。第二个中控的入口叫「微信」，所以发之前要换掉。
$script:from = 'console'
function NextLine([int]$timeoutMs) {
    if ($null -eq $script:outTask) { $script:outTask = $script:proc.StandardOutput.ReadLineAsync() }
    if (-not $script:outTask.Wait($timeoutMs)) { return $null }
    $line = $script:outTask.Result
    $script:outTask = $null
    return $line
}
# **发一条 op**：`authority` 是**调用方自己声明**的级别，中控按它的上限夹一次（§1.1）。
# 这里能靠它模拟"工具服务"（observe）和"智能体"（agent）来看矩阵认不认。
function Op([string]$corr, [string]$authority, [hashtable]$payload) {
    $script:seq++
    $m = [ordered]@{ id = "m-$script:seq"; ts = 0; type = 'request'; from = $script:from
                     to = 'ctrl'; corr = $corr; sid = $script:sid; authority = $authority; payload = $payload }
    # **中控要是已经死了，这里写不进去会抛** —— 而"它死了"本身就是一个判据结果，
    # 不该让整个脚本崩在半路（那样后面的判据一条都报不出来）。所以：写失败就当成
    # "没有回答"，交给判据去红。
    try {
        $script:proc.StandardInput.WriteLine(($m | ConvertTo-Json -Compress -Depth 12))
        $script:proc.StandardInput.Flush()
    } catch {
        return $null
    }
    $deadline = [DateTime]::UtcNow.AddMilliseconds(20000)
    while ([DateTime]::UtcNow -lt $deadline) {
        $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
        $line = NextLine $left
        if ($null -eq $line) { continue }
        if ($line.Trim() -eq '') { continue }
        $m2 = $line | ConvertFrom-Json
        if ($m2.type -eq 'result' -and $m2.corr -eq $corr) { return $m2 }
    }
    return $null
}
Start-Sleep -Seconds 2

Write-Host '== 只读那一档：谁都能看一眼 ==' -ForegroundColor Cyan
$p = Op 'c-ping' 'observe' @{ op = 'ping' }
Check '1. `ping` 通了，而且回的是**事实**（pong / last_seq / sid）—— 这个 op 到昨天还没有判据' {
    $p.payload.effect -eq 'confirmed' -and $p.payload.data.pong -eq $true `
        -and $p.payload.data.last_seq -ge 1 -and $p.payload.data.sid -eq $sid
}
$readOnly = @{ ping = @{ op = 'ping' }; list = @{ op = 'list' }; index = @{ op = 'index' }
               session = @{ op = 'session' }; prompts = @{ op = 'prompts' } }
$roBad = @()
foreach ($k in @('ping', 'list', 'index', 'session', 'prompts')) {
    $r = Op "c-ro-$k" 'observe' $readOnly[$k]
    if ($null -eq $r -or $r.payload.effect -ne 'confirmed') { $roBad += $k }
}
Check '2. 只读那一组在 **observe 档全通**（矩阵没把正常路堵上）' { $roBad.Count -eq 0 }
if ($roBad.Count -gt 0) { Write-Host "      拒了的：$($roBad -join ', ')" -ForegroundColor DarkGray }

Write-Host '== 会改状态或寿命的：低级别一律拒 ==' -ForegroundColor Cyan
$idsBefore = @((Op 'c-list-前' 'observe' @{ op = 'list' }).payload.data.instances | ForEach-Object { $_.id })
# **要拉起的是真的 echo**（不是一句"echo 个文件"）：这样"有没有偷偷动手"是**看得见的** ——
# 真拉起来的话，图上一定会多出一个实例。用一条注定失败的假命令反而验不出来。
$sp = Op 'c-spawn-低' 'observe' @{ op = 'spawn'; kind = 'echo'; cmd = @($echo) }
Start-Sleep -Milliseconds 500
$idsAfter = @((Op 'c-list-后' 'observe' @{ op = 'list' }).payload.data.instances | ForEach-Object { $_.id })
$newIds = @($idsAfter | Where-Object { $idsBefore -notcontains $_ })
Check '3. observe 档调 `spawn`：拒，说清要 user，而且**图上没多出实例**（真的没拉起来）' {
    # 后半句才是要害：只查"回了一句拒绝"的话，
    # 一个"先动手再回绝"的实现照样能骗过判据。而第 8 条是**同一个调用在 user 档**
    # （那边会真的多出一个实例）—— 两条对照着看，这条就不是空转。
    $sp.payload.effect -eq 'refused' -and $sp.payload.why -like '*user*' -and $newIds.Count -eq 0
}
$sd = Op 'c-shutdown-低' 'agent' @{ op = 'shutdown' }
$alive = Op 'c-alive' 'observe' @{ op = 'ping' }
Check '4. agent 档调 `shutdown`：拒，而且**中控还活着**' {
    $sd.payload.effect -eq 'refused' -and $sd.payload.why -like '*user*' `
        -and $alive.payload.effect -eq 'confirmed'
}
$st = Op 'c-stop-低' 'observe' @{ op = 'stop'; instance = 'echo:1' }
$still = Op 'c-still' 'observe' @{ op = 'list' }
$stillIds = @($still.payload.data.instances | ForEach-Object { $_.id })
Check '5. observe 档调 `stop`：拒，而且**那个服务还在**' {
    $st.payload.effect -eq 'refused' -and $st.payload.why -like '*user*' -and ($stillIds -contains 'echo:1')
}
# 一组"改状态 / 改寿命 / 改授权"的，全都要 user
$needsUser = @(
    @{ op = 'host'; path = $work }, @{ op = 'unhost'; path = $work }
    @{ op = 'delegate'; agent = 'agent:dada' }, @{ op = 'suspend' }, @{ op = 'resume' }
    @{ op = 'end' }, @{ op = 'prompt'; name = 'x' }, @{ op = 'grant'; agent = 'agent:dada' }
    @{ op = 'revoke'; agent = 'agent:dada' }
)
$leaks = @()
for ($i = 0; $i -lt $needsUser.Count; $i++) {
    $one = $needsUser[$i]
    $r = Op "c-u-$i" 'agent' $one
    if ($null -eq $r -or $r.payload.effect -ne 'refused' -or $r.payload.why -notlike '*user*') {
        $leaks += $one.op
    }
}
Check '6. 一串"改状态/寿命/授权"的 op 在 agent 档**全拒**，而且每句都说清要 user' {
    $leaks.Count -eq 0
}
if ($leaks.Count -gt 0) { Write-Host "      漏了的：$($leaks -join ', ')" -ForegroundColor DarkGray }
$rp = Op 'c-replay-低' 'agent' @{ op = 'replay'; agent = 'agent:dada' }
Check '7. `replay` 在低级别下**只能查自己**（不然它能读到整场会话的上下文）' {
    $rp.payload.effect -eq 'refused' -and $rp.payload.why -like '*只能查自己*'
}

Write-Host '== user 档照常：矩阵不许把正常路堵上 ==' -ForegroundColor Cyan
$added = Op 'c-spawn-高' 'user' @{ op = 'spawn'; kind = 'echo'; cmd = @($echo) }
$after = Op 'c-after' 'user' @{ op = 'list' }
$newId = $added.payload.data.instance_id
$ids2 = @($after.payload.data.instances | ForEach-Object { $_.id })
Check '8. user 档 `spawn` 照常：真的多了一个实例，而且它在图上' {
    $added.payload.effect -eq 'confirmed' -and $newId -and ($ids2 -contains $newId)
}
$stopped = Op 'c-stop-高' 'user' @{ op = 'stop'; instance = $newId }
Check '9. user 档 `stop` 照常：收得掉' {
    $stopped.payload.effect -eq 'confirmed'
}
$bye = Op 'c-bye' 'user' @{ op = 'shutdown' }
$exited = $proc.WaitForExit(8000)
Check '10. user 档 `shutdown` 是真的关（回 bye，进程真的退出）' {
    $bye.payload.effect -eq 'confirmed' -and $bye.payload.data.bye -eq $true -and $exited
}
if (-not $proc.HasExited) { $proc.Kill() }
$errText = $proc.StandardError.ReadToEnd()

Write-Host '== 上限与矩阵是一对：agent 档的入口 ==' -ForegroundColor Cyan
# **第二個中控**：这条口子配成 `--channel 微信:agent`。
# 为什么非得另起一个进程：**上限是进程级配置**（一个中控只有一个入口），
# 而"降级"只能在 `上限 < 声称的级别` 时发生 —— 那种入口**发不出**上面那些 user 档 op。
# （这三条原来在 `smoke-session.ps1` 里，2026-09-23 搬过来：那边要用 user 入口走完会话流程。）
$capWork = Join-Path $work 'cap'
New-Item -ItemType Directory -Path $capWork -Force | Out-Null
$capData = Join-Path $capWork 'data'
$capServices = Join-Path $capWork 'services.json'
Set-Content -LiteralPath $capServices -Value '[]' -Encoding utf8
$psi2 = New-Object System.Diagnostics.ProcessStartInfo
$psi2.FileName = $ctrl
$psi2.Arguments = "--sid s-cap --data-dir `"$capData`" --services `"$capServices`" --channel 微信:agent --tick-ms 0"
$psi2.RedirectStandardInput = $true
$psi2.RedirectStandardOutput = $true
$psi2.RedirectStandardError = $true
$psi2.UseShellExecute = $false
$psi2.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
$script:proc = [System.Diagnostics.Process]::Start($psi2)
$script:outTask = $null
$script:from = '微信'
Start-Sleep -Seconds 2
$capList = Op 'k-list' 'user' @{ op = 'list' }
$capCh = @($capList.payload.data.instances | Where-Object { $_.kind -eq 'channel' })
Check '11. 入口的权威上限是**配置出来的**：这条通道是 agent（不是写死的 user）' {
    $capCh.Count -eq 1 -and $capCh[0].id -eq '微信' -and $capCh[0].ceiling -eq 'agent'
}
$capEnd = Op 'k-end' 'user' @{ op = 'end' }
Check '12. 同一个 op 在 agent 上限的入口上**被拒**（矩阵与上限是一对）' {
    $capEnd.payload.effect -eq 'refused' -and $capEnd.payload.why -like '*user*'
}
# **先落盘再读日志**：日志是中控独占的，它手里还压着写缓冲 —— 直接读文件会读到旧的。
# （`barrier` 是 agent 档的 op，这条通道发得出去 —— 顺带把那条也验了。）
$null = Op 'k-barrier' 'agent' @{ op = 'barrier'; reason = '读日志之前' }
$capLog = Join-Path (Join-Path (Join-Path $capData 'sessions') 's-cap') 'log.jsonl'
$capRecs = @(Get-Content -LiteralPath $capLog | Where-Object { $_.Trim() -ne '' } |
    ForEach-Object { $_ | ConvertFrom-Json })
$fromCh = @($capRecs | Where-Object { $_.from -eq '微信' -and $_.type -eq 'request' })
Check '13. **只准降**：自称 user 的消息从 agent 上限的通道进来，**日志里记的是 agent**' {
    ($fromCh.Count -ge 2) -and (($fromCh | Where-Object { $_.authority -ne 'agent' }).Count -eq 0)
}
$script:proc.StandardInput.Close()
$script:proc.WaitForExit(8000) | Out-Null
$capErr = $script:proc.StandardError.ReadToEnd()
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    Write-Host '== stderr ==' -ForegroundColor DarkGray
    $errText -split "`n" | ForEach-Object { if ($_.Trim()) { Write-Host ('  ' + $_) -ForegroundColor DarkGray } }
    $capErr -split "`n" | ForEach-Object { if ($_.Trim()) { Write-Host ('  ' + $_) -ForegroundColor DarkGray } }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host ("工作目录留在：" + $work) -ForegroundColor DarkGray
