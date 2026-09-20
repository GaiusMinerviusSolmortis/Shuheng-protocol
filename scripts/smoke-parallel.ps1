# **交错的那条路**：一次发一批、不为每条等回包（2026-09-20）。
#
#   pwsh -File scripts/smoke-parallel.ps1
#
# 为什么要有它：**其余 33 个脚本全是一问一答**（连发的最多三行，在 `smoke-contract` 里）。
# 而真实的运行时不长那样 —— 智能体、GUI、探针、别的会话是**同时在说话**的。
# README 里早写过一句："异步工具的脚本必须一问一答地喂，否则会全撞 `busy`" ——
# 那句话是**关于终端那一个异步工具**的测法提醒，不是"整个系统只许一问一答"。
#
# 这条路上的不变量是协议 §3 那一条：**一次 request 恰好一个 result，corr 一一对上**。
# 在交错下它比单发时脆：串台（A 的结果回给 B）、吞掉（谁都没收到）、
# 多发（一条请求两个结果）都只在这种时候露出来。
#
# 判据的写法也照着这条思路：**每条命令带一个只有它自己有的记号**，
# 然后查"这条的回包里是不是这条的记号" —— 串台就红。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$echo = Join-Path $root 'target\debug\harness-echo.exe'
$editor = Join-Path $root 'target\debug\harness-editor.exe'
$term = Join-Path $root 'target\debug\harness-term.exe'
foreach ($b in @($ctrl, $echo, $editor, $term)) { if (-not (Test-Path $b)) { throw "产物不在：$b" } }
function J([string]$p) { return ($p -replace '\\', '\\') }
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-parallel-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null
Set-Content -LiteralPath (Join-Path $project 'a.txt') -Value "hello`n世界" -Encoding utf8
$sid = 's-par'
$dataDir = Join-Path $work 'data'
$servicesPath = Join-Path $work 'services.json'
@"
[
  { "kind": "echo",   "id": "echo:1",   "role": "tool", "cmd": ["$(J $echo)"] },
  { "kind": "editor", "id": "editor:1", "role": "tool", "cmd": ["$(J $editor)"],
    "args": ["--fence", "$(J $project)"] },
  { "kind": "term",   "id": "term:1",   "role": "tool", "cmd": ["$(J $term)"],
    "args": ["--fence", "$(J $project)"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ctrl
$psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 6000 --tick-ms 0"
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
$proc = [System.Diagnostics.Process]::Start($psi)
$script:outTask = $null
$script:seq = 0
$script:all = New-Object System.Collections.Generic.List[string]

function NextLine([int]$timeoutMs) {
    if ($null -eq $script:outTask) { $script:outTask = $proc.StandardOutput.ReadLineAsync() }
    if (-not $script:outTask.Wait($timeoutMs)) { return $null }
    $line = $script:outTask.Result
    $script:outTask = $null
    if ($null -ne $line) { $script:all.Add($line.Trim()) | Out-Null }
    return $line
}
function Send([string]$to, [string]$corr, [hashtable]$payload) {
    $script:seq++
    $m = [ordered]@{ id = "m-$($script:seq)"; ts = 0; type = 'request'
                     from = 'console'; to = $to; corr = $corr; sid = $sid
                     authority = 'user'; payload = $payload }
    $proc.StandardInput.WriteLine(($m | ConvertTo-Json -Compress -Depth 12))
    $proc.StandardInput.Flush()
}
function WaitCorr([string]$corr, [int]$timeoutMs = 30000) {
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
function Ask([string]$to, [string]$corr, [hashtable]$payload) {
    Send $to $corr $payload
    return WaitCorr $corr
}

Write-Host '== 先把三个实例开起来（这三条也走一遍普通路）==' -ForegroundColor Cyan
$r0 = Ask 'echo:1' 'w0' @{ tool = 'ping'; args = @{} }
$r1 = Ask 'editor:1' 'w1' @{ tool = 'open'; args = @{ path = 'a.txt' } }
$r2 = Ask 'term:1' 'w2' @{ tool = 'open'; args = @{ path = '.'; purpose = '交错压测' } }
# ⚠️ `instance_id` 在**信封的 payload 里**（不是顶层）—— 第一版读成顶层，
# 于是下面那 4 条 `read` / 6 条 `run` 全是空 id（正好被新加的那道门拦成 `invalid`），
# 判据 5 就红了：**那次红是脚本的锅，不是被测对象的锅**。
$docId = $r1.payload.instance_id
$termId = $r2.payload.instance_id
Write-Host "  echo/editor/term 都起来了（文档 $docId，终端 $termId）" -ForegroundColor DarkGray

Write-Host '== 一次发 24 条，中间不等回包 ==' -ForegroundColor Cyan
# **每条命令/每条 echo 都带一个只有它自己有的记号** —— 串台就红。
# 顺序是**轮着来**的：三个服务交替，故意让回包顺序和发送顺序不一样。
$burst = New-Object System.Collections.Generic.List[object]
for ($i = 1; $i -le 8; $i++) {
    $burst.Add(@{ to = 'echo:1';   corr = "p-echo-$i";   mark = "MARK-ECHO-$i";   payload = @{ tool = 'echo'; args = @{ text = "MARK-ECHO-$i" } } }) | Out-Null
}
for ($i = 1; $i -le 6; $i++) {
    $burst.Add(@{ to = 'editor:1'; corr = "p-edst-$i";   mark = '';              payload = @{ tool = 'status' } }) | Out-Null
}
for ($i = 1; $i -le 4; $i++) {
    $burst.Add(@{ to = 'editor:1'; corr = "p-edread-$i"; mark = '';              payload = @{ tool = 'read'; args = @{ instance_id = $docId; limit = 6 } } }) | Out-Null
}
for ($i = 1; $i -le 6; $i++) {
    $burst.Add(@{ to = 'term:1';   corr = "p-run-$i";    mark = "MARK-RUN-$i";    payload = @{ tool = 'run'; args = @{ instance_id = $termId; cmd = "echo MARK-RUN-$i" } } }) | Out-Null
}
$order = @($burst | Sort-Object { Get-Random })   # 打乱，让"发送顺序"和"服务顺序"不一样
$sent = @{}
foreach ($b in $order) { $sent[$b.corr] = $true; Send $b.to $b.corr $b.payload }
Write-Host "  发了 $($sent.Count) 条（$($sent.Keys -join ' ')）" -ForegroundColor DarkGray

# 收：直到每条 corr 都有 result，或者超时
$got = @{}
$arrival = New-Object System.Collections.Generic.List[string]
$deadline = [DateTime]::UtcNow.AddSeconds(60)
while ([DateTime]::UtcNow -lt $deadline -and $got.Count -lt $sent.Count) {
    $line = NextLine 2000
    if ($null -eq $line) { continue }
    if ($line.Trim() -eq '') { continue }
    $m = $line | ConvertFrom-Json
    if ($m.type -ne 'result') { continue }
    $arrival.Add($m.corr) | Out-Null
    if ($got.ContainsKey($m.corr)) { $got[$m.corr] = 'TWICE' }
    else {
        $by = $burst | Where-Object { $_.corr -eq $m.corr } | Select-Object -First 1
        $got[$m.corr] = [pscustomobject]@{ msg = $m; mark = $by.mark; to = $by.to }
    }
}

Write-Host '== 数一遍 ==' -ForegroundColor Cyan
$missing = @($sent.Keys | Where-Object { -not $got.ContainsKey($_) })
$twice = @($got.Keys | Where-Object { $got[$_] -eq 'TWICE' })
$strangers = @($arrival | Where-Object { -not $sent.ContainsKey($_) } | Select-Object -Unique)
# 发送顺序（供后面比"到达顺序"用；**放在 Check 外面** —— 写在 scriptblock 里就取不到了）
$sendOrder = @($order | ForEach-Object { $_.corr })

Check '1. **一条不落**：24 条请求全部拿到 result' {
    $missing.Count -eq 0
}
Check '2. **一条不多**：没有哪个 corr 拿到两条 result' {
    $twice.Count -eq 0
}
Check '3. **没有孤儿**：收到的每条 result 都对得上发出去的某一条' {
    $strangers.Count -eq 0
}
Check '4. **结果不许串台**（echo）：每条回包里是**它自己**那个记号' {
    $bad = @()
    foreach ($k in $got.Keys) {
        $g = $got[$k]
        if ($null -eq $g -or $g -eq 'TWICE') { continue }
        if ($g.to -ne 'echo:1') { continue }
        $txt = $g.msg.payload | ConvertTo-Json -Depth 8 -Compress
        if ($txt -notlike "*$($g.mark)*") { $bad += $k }
    }
    $bad.Count -eq 0
}
Check '5. **结果不许串台**（term）：跑成了的那几条，回包里只有**它自己**那个记号' {
    $bad = @()
    foreach ($k in $got.Keys) {
        $g = $got[$k]
        if ($null -eq $g -or $g -eq 'TWICE') { continue }
        if ($g.to -ne 'term:1') { continue }
        $p = $g.msg.payload
        # ⚠️ **只查真的跑成了的那几条**：撞 `busy` 的回包会引用"现在正在跑的那条命令"，
        # 于是它**合法地**带着别人的记号（第一版没分开，判据 5 因此假红）。
        if ($p.code -eq 'busy') { continue }
        $txt = $p | ConvertTo-Json -Depth 8 -Compress
        # 它自己那个记号必须在
        if ($txt -notlike "*$($g.mark)*") { $bad += $k; continue }
        # **别的**记号一个都不许在
        foreach ($other in ($burst | Where-Object { $_.to -eq 'term:1' -and $_.corr -ne $k })) {
            if ($txt -like "*$($other.mark)*") { $bad += $k }
        }
    }
    $bad.Count -eq 0
}
Check '6. 撞 `busy` 的那几条**也是正常答案**：带出口，不是沉默' {
    $busyGot = @($got.Keys | Where-Object { $got[$_] -ne 'TWICE' -and $got[$_].msg.payload.code -eq 'busy' })
    # 一条 busy 都没有也行（命令跑得太快时很正常）—— 但**有**的话，它必须带 remedy
    @($busyGot | Where-Object { -not $got[$_].msg.payload.remedy }).Count -eq 0
}

$proc.StandardInput.Close()
$proc.WaitForExit(15000) | Out-Null
$drain = $proc.StandardOutput.ReadToEndAsync()
$drain.Wait(5000) | Out-Null
if ($drain.IsCompleted -and $drain.Result) {
    foreach ($l in ($drain.Result -split "`n")) { if ($l.Trim() -ne '') { $script:all.Add($l.Trim()) | Out-Null } }
}
$errText = $proc.StandardError.ReadToEndAsync()
$errText.Wait(4000) | Out-Null
$err = if ($errText.IsCompleted) { $errText.Result } else { '' }

$records = @($script:all | Where-Object { $_.StartsWith('{') } | ForEach-Object { $_ | ConvertFrom-Json })
$results = @($records | Where-Object { $_.type -eq 'result' })
$logPath = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl'
$log = @(Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json })

Check '7. 回包顺序**真的乱过**（不然这条脚本只是把一问一答抄了一遍）' {
    # 发送顺序 vs 到达顺序：至少有一处不同。这不是"性能"，是**这条判据的前提**。
    $diff = $false
    for ($i = 0; $i -lt [Math]::Min($sendOrder.Count, $arrival.Count); $i++) {
        if ($sendOrder[$i] -ne $arrival[$i]) { $diff = $true; break }
    }
    $diff
}
Check '8. **日志里也一一对上**：每个 corr 恰好一条 request + 一条 result（不变量二的地基）' {
    $bad = @()
    foreach ($k in $sent.Keys) {
        $req = @($log | Where-Object { $_.type -eq 'request' -and $_.corr -eq $k }).Count
        $res = @($log | Where-Object { $_.type -eq 'result' -and $_.corr -eq $k }).Count
        if ($req -ne 1 -or $res -ne 1) { $bad += "$k(req=$req,res=$res)" }
    }
    $bad.Count -eq 0
}
Check '9. 交错下**没有一条结果违反自己声明的形**（`schema_violation` 为 0）' {
    @($records | Where-Object { $_.payload.schema_violation }).Count -eq 0
}
Check '10. 全程没有 `service_down` / `unresponsive`（三个服务都活到了最后）' {
    @($records | Where-Object { $_.payload.code -eq 'service_down' -or $_.payload.code -eq 'service_unresponsive' }).Count -eq 0
}

Write-Host ''
Write-Host '== 交错的样子（供人看，不是判据）==' -ForegroundColor Cyan
Write-Host "  发送顺序：$($sendOrder -join ' ')" -ForegroundColor DarkGray
Write-Host "  到达顺序：$($arrival -join ' ')" -ForegroundColor DarkGray
$busy = @($got.Values | Where-Object { $_ -ne 'TWICE' -and $_.msg.payload.code -eq 'busy' })
Write-Host "  其中撞 busy 的：$($busy.Count) 条（**撞 busy 也是正常答案** —— 它带出口，不是沉默）" -ForegroundColor DarkGray

Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    Write-Host '== stderr ==' -ForegroundColor DarkGray
    foreach ($l in ($err -split "`n")) { if ($l.Trim() -ne '') { Write-Host "  $l" -ForegroundColor DarkGray } }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
