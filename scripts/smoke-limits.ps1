# **终端那棵树的硬限额**（§5.8 决定二那张表里的"资源"面，2026-09-20）。
#
#   pwsh -File scripts/smoke-limits.ps1
#
# 在这之前终端的资源面只有**超时** —— 而超时是**墙钟**：一条命令可以在 30 秒里
# 吃掉 8 GB 内存、起 2000 个进程，然后**正常地**在超时前结束。
#
# 现在三个限额关在同一个 Job Object 上（**由内核执行**，不是我们读输出时自己数的）：
#   内存（提交内存）/ 进程数 / CPU 时间。
#
# ⚠️ **三条"发作"的方式不一样，判据也必须分开**（第一版我就写错过）：
#   · **CPU 时间**超了 ⇒ **内核终止整棵树** —— 这条**是杀**，判据可以硬；
#   · **进程数**超了 ⇒ 再创建就**失败**（命令自己报错）—— 不是杀；
#   · **内存**超了 ⇒ **命令自己分配失败** —— **内核根本没杀它**。
#   把三条写成一种，就会得出"它被内存限额杀了"这种**假话**。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$term = Join-Path $root 'target\debug\harness-term.exe'
foreach ($b in @($ctrl, $term)) { if (-not (Test-Path $b)) { throw "产物不在：$b" } }
function J([string]$p) { return ($p -replace '\\', '\\') }
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-limits-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
$project2 = Join-Path $work 'project2'
New-Item -ItemType Directory -Path $project -Force | Out-Null
New-Item -ItemType Directory -Path $project2 -Force | Out-Null
$sid = 's-lim'
$dataDir = Join-Path $work 'data'
$servicesPath = Join-Path $work 'services.json'
# **开了限额的那个终端**：内存 128 MB、最多 4 个进程、CPU 时间 1.5 秒
@"
[
  { "kind": "term", "id": "term:1", "role": "tool",
    "cmd": ["$(J $term)"],
    "args": ["--fence", "$(J $project)",
             "--mem-limit-mb", "128", "--max-procs", "4", "--cpu-time-ms", "1500"] },
  { "kind": "term", "id": "term:2", "role": "tool",
    "cmd": ["$(J $term)"],
    "args": ["--fence", "$(J $project2)"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ctrl
$psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 5000 --tick-ms 0"
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
$proc = [System.Diagnostics.Process]::Start($psi)
$script:outTask = $null
$script:seq = 0
function NextLine([int]$timeoutMs) {
    if ($null -eq $script:outTask) { $script:outTask = $proc.StandardOutput.ReadLineAsync() }
    if (-not $script:outTask.Wait($timeoutMs)) { return $null }
    $line = $script:outTask.Result
    $script:outTask = $null
    return $line
}
function Send([string]$to, [string]$corr, [hashtable]$payload) {
    $script:seq++
    $m = [ordered]@{ id = "m-$($script:seq)"; ts = 0; type = 'request'; from = 'console'
                     to = $to; corr = $corr; sid = $sid; authority = 'user'; payload = $payload }
    $proc.StandardInput.WriteLine(($m | ConvertTo-Json -Compress -Depth 12))
    $proc.StandardInput.Flush()
}
function WaitCorr([string]$corr, [int]$timeoutMs = 60000) {
    $deadline = [DateTime]::UtcNow.AddMilliseconds($timeoutMs)
    while ([DateTime]::UtcNow -lt $deadline) {
        $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
        $line = NextLine $left
        if ($null -eq $line) { continue }
        if ($line.Trim() -eq '') { continue }
        $m = $line | ConvertFrom-Json
        if ($m.type -eq 'result' -and $m.corr -eq $corr) { return $m }
    }
    return $null
}
for ($i = 0; $i -lt 25; $i++) {
    Start-Sleep -Milliseconds 150
    Send 'ctrl' "ready-$i" @{ op = 'list' }
    $r = WaitCorr "ready-$i" 900
    if ($r -and (@($r.payload.data.instances | ForEach-Object { $_.id }) -contains 'term:1')) { break }
}

Write-Host '== 判据 ==' -ForegroundColor Cyan
# ⚠️ **三条命令都用单引号包**：双引号里 PowerShell 会先把 `$t` / `$i` 吃掉，
#    于是真正跑起来的是一条被掏空的命令 —— 它会立刻报错退出，
#    而现象是"CPU 只用了 171 ms"，看起来像限额没生效。（第一版就是这么错的。）
$cpuHog = 'powershell -NoProfile -Command "$t=Get-Date; while(((Get-Date)-$t).TotalSeconds -lt 30){$x=1.0; for($i=0;$i -lt 300000;$i++){$x=$x*1.000001}}; Write-Output never"'
$manyProcs = 'for /L %i in (1,1,6) do start /B cmd /C "ping -n 3 127.0.0.1 > NUL"'
$shortWork = 'powershell -NoProfile -Command "$x=1.0; for($i=0;$i -lt 300000;$i++){$x=$x*1.000001}; Write-Output done"'
# **CPU 时间那条**：忙循环 30 秒，但 CPU 上限只有 1.5 秒 ⇒ 内核到点终止整棵树
Send 'term:1' 'o1' @{ tool = 'open'; args = @{ path = '.'; purpose = 'CPU 限额' } }
$o1 = WaitCorr 'o1'
Send 'term:1' 'r1' @{ tool = 'run'; args = @{
    instance_id = 't-1'
    cmd = $cpuHog
    timeout_ms = 25000 } }
$r1 = WaitCorr 'r1' 60000
# **进程数那条**：一次起 6 个，上限 4 ⇒ 后面的创建会失败
Send 'term:1' 'r2' @{ tool = 'run'; args = @{
    instance_id = 't-1'
    cmd = $manyProcs
    timeout_ms = 15000 } }
$r2 = WaitCorr 'r2' 40000
# **没开限额的那个**：同一条忙循环命令（只不过短一点）能跑完
Send 'term:2' 'o2' @{ tool = 'open'; args = @{ path = '.'; purpose = '没开限额' } }
$o2 = WaitCorr 'o2'
Send 'term:2' 'r3' @{ tool = 'run'; args = @{
    instance_id = 't-1'
    cmd = $shortWork
    timeout_ms = 20000 } }
$r3 = WaitCorr 'r3' 40000

$proc.StandardInput.Close()
$proc.WaitForExit(8000) | Out-Null
try { $d = $proc.StandardOutput.ReadToEndAsync(); $d.Wait(2000) | Out-Null } catch { }
$et = $proc.StandardError.ReadToEndAsync(); $et.Wait(4000) | Out-Null
$errText = if ($et.IsCompleted) { $et.Result } else { '' }
$logPath = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl'
$records = Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json }
$presence = $records | Where-Object { $_.type -eq 'presence' -and $_.from -eq 'term:1' -and $_.payload.manifest } |
    Select-Object -First 1
$presence2 = $records | Where-Object { $_.type -eq 'presence' -and $_.from -eq 'term:2' -and $_.payload.manifest } |
    Select-Object -First 1

Check '1. **声明跟着事实走**：开了限额的服务报 `resources: limits`，没开的还是 `timeout`' {
    $presence.payload.manifest.sandbox.resources -eq 'limits' -and `
        $presence2.payload.manifest.sandbox.resources -eq 'timeout'
}
Check '2. 那个开着限额的终端**过了注册关**（`limits` 在闭集里）' {
    $errText -notlike '*sandbox.resources*' -and $errText -notlike '*manifest 不合规*'
}
Check '3. **CPU 时间上限真的把整棵树掐了**（不是它自己退的）' {
    $r1 -and $r1.payload.effect -eq 'confirmed' -and `
        $r1.payload.limits -like '*撞了 CPU 时间上限*' -and `
        $r1.payload.limits -like '*内核到点终止整棵树*'
}
Check '4. 那句账里带着**测量值**（用掉多少 CPU / 上限多少）' {
    $r1.payload.limits -like '*用掉*ms CPU 时间（上限 1500 ms）*'
}
Check '5. **内存那条不说"被杀"** —— 它只挡分配，内核不杀它' {
    # 这一条盯的是措辞：把三个限额当成一种发作方式，就会写出假话
    $r1.payload.limits -notlike '*被内存*杀*'
}
Check '6. 进程数顶到上限时，判词说清是**创建失败**不是被杀' {
    $r2 -and $r2.payload.limits -ne $null -and `
        ($r2.payload.limits -like '*进程数顶到上限*' -or $r2.payload.limits -like '*一共起过*')
}
Check '7. **没开限额的终端一切照旧**：同一条命令跑得完，而且结果里没有那笔账' {
    $r3 -and $r3.payload.effect -eq 'confirmed' -and $r3.payload.output -like '*done*' -and `
        $null -eq $r3.payload.limits
}
Check '8. 这一路没有任何一条结果**违反自己声明的形**（`limits` 那格声明过了）' {
    @($records | Where-Object { $_.payload.schema_violation }).Count -eq 0
}

Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    Write-Host '== stderr ==' -ForegroundColor DarkGray
    foreach ($l in ($errText -split "`n")) { if ($l.Trim() -ne '') { Write-Host "  $l" -ForegroundColor DarkGray } }
    Write-Host '== CPU 那条的判词 ==' -ForegroundColor DarkGray
    if ($r1) { Write-Host "  $($r1.payload.limits)" -ForegroundColor DarkGray }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host '== 那两笔账 ==' -ForegroundColor Cyan
if ($r1) { Write-Host "  CPU：$($r1.payload.limits)" }
if ($r2) { Write-Host "  进程：$($r2.payload.limits)" }
Write-Host "工作目录留在：$work"
