# 阶段 2 的第二半：第二个工具实例 —— 终端。
#
#   pwsh -File scripts/smoke-term.ps1
#
# 它检验的是"这套抽象能不能撑住第二个工具实例"：
# 同一个中控、同一套信封、同一个围栏件，换一个完全不同的域（shell）。
#
# 注意：这个脚本必须一问一答地喂。`run` 是异步的（先流式 chunk、最后 result），
# 把一批请求一次性灌进去的话，后面的会在第一条命令跑完之前到齐，于是全部撞上 busy ——
# 那不是被测对象的问题，是测法的问题。真实的智能体也正是"发一条、等结果、再发下一条"。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$term = Join-Path $root 'target\debug\harness-term.exe'
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
if (-not (Test-Path $term)) { throw "产物不在：$term" }
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-term-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path (Join-Path $project 'sub') -Force | Out-Null
Set-Content -LiteralPath (Join-Path $project 'hello.txt') -Value 'hello from project' -Encoding utf8
$sid = 's-term'
$dataDir = Join-Path $work 'data'
$servicesPath = Join-Path $work 'services.json'
@"
[
  { "kind": "term", "id": "term:1", "cmd": ["$(($term -replace '\\','\\'))"], "args": ["--fence", "$($project -replace '\\','\\')"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
$script:all = New-Object System.Collections.Generic.List[string]
$script:replies = @{}
$script:seq = 0
$script:outTask = $null
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ctrl
$psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 8000"
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
# **必须显式设成 UTF-8。** 它的默认值是系统 ANSI 代码页（中文 Windows 上是 GBK），
# 于是中文会被编成 GBK 字节 —— 协议是 UTF-8 的 NDJSON，那样会在
# "不是合法的 UTF-8" 上栽跟头（这次就被它咬了）。
$psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
$proc = [System.Diagnostics.Process]::Start($psi)
# 注意：不要用 add_OutputDataReceived + 脚本块 —— 那个回调跑在别的线程上、
# 没有 Runspace，会直接让整个 pwsh 崩掉。用 ReadLineAsync + 轮询 Task，
# 既有超时又不需要回调。
function NextLine([int]$timeoutMs) {
    if ($null -eq $script:outTask) { $script:outTask = $proc.StandardOutput.ReadLineAsync() }
    if (-not $script:outTask.Wait($timeoutMs)) { return $null }
    $line = $script:outTask.Result
    $script:outTask = $null
    return $line
}
function Send([string]$to, [string]$corr, [hashtable]$payload) {
    $script:seq++
    $msg = [ordered]@{
        id = "m-$($script:seq)"; ts = 0; type = 'request'
        from = 'console'; to = $to; corr = $corr; sid = $sid
        authority = 'user'; payload = $payload
    }
    $json = $msg | ConvertTo-Json -Compress -Depth 12
    if ($env:HARNESS_SMOKE_DEBUG) { Write-Host "    → 发送 $json" }
    $proc.StandardInput.WriteLine($json)
    $proc.StandardInput.Flush()
}
function Wait([string]$corr, [int]$timeoutMs = 20000) {
    # 先看有没有已经到过的：c-18 的结果可能排在 c-16 前面（关终端时先给被中断的调用收尾，
    # 再回 close 自己）。丢掉的不是消息，是测试自己的记账。
    if ($script:replies.ContainsKey($corr)) { return $script:replies[$corr] }
    $deadline = [DateTime]::UtcNow.AddMilliseconds($timeoutMs)
    while ([DateTime]::UtcNow -lt $deadline) {
        $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
        $line = NextLine $left
        if ($null -eq $line) { continue }
        if ($line.Trim() -eq '') { continue }
        $script:all.Add($line)
        $m = $line | ConvertFrom-Json
        if ($env:HARNESS_SMOKE_DEBUG) { Write-Host "    · 读到 $($m.type) corr=$($m.corr) from=$($m.from)" }
        if ($m.type -eq 'result') { $script:replies[$m.corr] = $m }
        if ($m.type -eq 'result' -and $m.corr -eq $corr) { return $m }
    }
    throw "等 $corr 的结果超时了（$timeoutMs ms）"
}
function Ask([string]$to, [string]$corr, [hashtable]$payload, [int]$timeoutMs = 20000) {
    Send $to $corr $payload
    return Wait $corr $timeoutMs
}
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
Write-Host "== 跑中控（会话 $sid，托管 $project）==" -ForegroundColor Cyan
$r01 = Ask 'term:1' 'c-01' @{ tool = 'open'; args = @{ path = '.'; purpose = '跑测试与看构建' } }
$r02 = Ask 'term:1' 'c-02' @{ tool = 'run'; args = @{ instance_id = 't-1'; cmd = 'echo HELLO-TERM' } }
$r03 = Ask 'term:1' 'c-03' @{ tool = 'run'; args = @{ instance_id = 't-1'; cmd = 'cd' } }
$r04 = Ask 'term:1' 'c-04' @{ tool = 'run'; args = @{ instance_id = 't-1'; cmd = 'cd sub' } }
$r05 = Ask 'term:1' 'c-05' @{ tool = 'run'; args = @{ instance_id = 't-1'; cmd = 'cd ..' } }
$r06 = Ask 'term:1' 'c-06' @{ tool = 'run'; args = @{ instance_id = 't-1'; cmd = 'cd ../../../../Windows' } }
$r07 = Ask 'term:1' 'c-07' @{ tool = 'run'; args = @{ instance_id = 't-1'; cmd = 'cd C:\Windows' } }
$r08 = Ask 'term:1' 'c-08' @{ tool = 'run'; args = @{ instance_id = 't-1'; cmd = 'cmd /c exit 3' } }
$r09 = Ask 'term:1' 'c-09' @{ tool = 'run'; args = @{ instance_id = 't-1'; cmd = 'ping -n 30 127.0.0.1'; timeout_ms = 800 } }
$r10 = Ask 'term:1' 'c-10' @{ tool = 'run'; args = @{ instance_id = 't-1'; cmd = 'echo SECOND-READ' } }
$r11 = Ask 'term:1' 'c-11' @{ tool = 'read'; args = @{ instance_id = 't-1' } }
$r12 = Ask 'term:1' 'c-12' @{ tool = 'read'; args = @{ instance_id = 't-1' } }
$r13 = Ask 'term:1' 'c-13' @{ tool = 'send'; args = @{ instance_id = 't-1'; text = 'x' } }
$r14 = Ask 'term:1' 'c-14' @{ tool = 'status' }
$r15 = Ask 'term:1' 'c-15' @{ tool = 'open'; args = @{ path = 'sub'; purpose = '第二条终端' } }
Send 'term:1' 'c-16' @{ tool = 'run'; args = @{ instance_id = 't-2'; cmd = 'ping -n 30 127.0.0.1' } }
Start-Sleep -Milliseconds 400
$r17 = Ask 'term:1' 'c-17' @{ tool = 'run'; args = @{ instance_id = 't-2'; cmd = 'echo SHOULD-BE-BUSY' } }
$r18 = Ask 'term:1' 'c-18' @{ tool = 'close'; args = @{ instance_id = 't-2' } }
$r16 = Wait 'c-16' 10000
$r19 = Ask 'term:1' 'c-19' @{ tool = 'close'; args = @{ instance_id = 't-1' } }
$proc.StandardInput.Close()
$proc.WaitForExit(15000) | Out-Null
$drain = $proc.StandardOutput.ReadToEndAsync()
$drain.Wait(5000) | Out-Null
if ($drain.IsCompleted -and $drain.Result) {
    foreach ($l in ($drain.Result -split "`n")) { if ($l.Trim() -ne '') { $script:all.Add($l.Trim()) } }
}
$errText = $proc.StandardError.ReadToEndAsync()
$errText.Wait(5000) | Out-Null
$script:errText = if ($errText.IsCompleted) { $errText.Result } else { '' }
Write-Host '== 判据 ==' -ForegroundColor Cyan
Check '1. open 拿到终端实例，并且带一句用途说明' {
    $r01.payload.effect -eq 'confirmed' -and $r01.payload.instance_id -eq 't-1' -and $r01.payload.purpose -eq '跑测试与看构建'
}
Check '2. run 是异步的：先流式 chunk，最后才有 result' {
    $chunks = @($script:all | ForEach-Object { $_ | ConvertFrom-Json } | Where-Object { $_.type -eq 'chunk' -and $_.corr -eq 'c-02' })
    ($chunks.Count -ge 1) -and ($r02.payload.exit_code -eq 0) -and ($r02.payload.output -like '*HELLO-TERM*')
}
Check '3. 裸 cd 报当前目录' { $r03.payload.effect -eq 'confirmed' -and $r03.payload.cwd -like '*project' }
Check '4. 路径内 cd 可以' { $r04.payload.effect -eq 'confirmed' -and $r04.payload.cwd -like '*project*sub' }
Check '5. cd .. 回到托管根也可以（仍在围栏内）' {
    $r05.payload.effect -eq 'confirmed' -and $r05.payload.cwd -like '*project' -and $r05.payload.cwd -notlike '*sub*'
}
Check '6. cd 出去被围栏拦下（相对 .. 绕）' {
    $r06.payload.effect -eq 'refused' -and $r06.payload.code -eq 'refused' -and $r06.payload.why -like '*不在托管路径内*' -and $r06.payload.remedy -like '*不允许 cd 出去*'
}
Check '7. cd 出去被围栏拦下（绝对路径）' { $r07.payload.effect -eq 'refused' -and $r07.payload.code -eq 'refused' }
Check '8. 命令失败 ≠ 调用失败：exit 3 但 effect 仍是 confirmed' {
    $r08.payload.effect -eq 'confirmed' -and $r08.payload.exit_code -eq 3
}
Check '9. 超时被掐掉，并且诚实报 partial + timeout' {
    $r09.payload.effect -eq 'partial' -and $r09.payload.code -eq 'timeout' -and $r09.payload.remedy
}
Check '10. 超时之后终端还能接着用' {
    $r10.payload.effect -eq 'confirmed' -and $r10.payload.output -like '*SECOND-READ*'
}
Check '11. read 默认给「自上次以来」的增量，读完游标就空了' {
    ($r11.payload.text.Length -gt 0) -and ($r12.payload.text -eq '')
}
Check '12. 会话模式的 send 被明确拒绝（不假装支持）' {
    $r13.payload.effect -eq 'refused' -and $r13.payload.code -eq 'invalid' -and $r13.payload.why -like '*会话模式*' -and $r13.payload.remedy -like '*PTY*'
}
Check '13. status 看得见终端、cwd、用途、持有者' {
    $t = $r14.payload.data.terms | Where-Object { $_.instance_id -eq 't-1' }
    # 字段叫 `owner`（谁开的这个实例）—— 2026-09-19 与实例事件对齐时改的名，
    # 原来叫 `holder`，和"写锁的持有者"（编辑器的 `held_by`）容易混
    ($r14.payload.data.term_count -eq 1) -and ($t.purpose -eq '跑测试与看构建') -and ($t.owner -like 'console@*') -and ($t.cwd -like '*project')
}
Check '14. 实例内部串行：跑着的时候再来一条是 busy（并且说清谁在跑）' {
    $r17.payload.effect -eq 'refused' -and $r17.payload.code -eq 'busy' -and $r17.payload.running.cmd -like '*ping*' -and $r17.payload.remedy
}
Check '15. 关掉正在跑命令的终端时，那条命令不被静默丢弃' {
    $r16.payload.effect -eq 'partial' -and $r16.payload.code -eq 'canceled' -and $r16.payload.remedy -and $r18.payload.killed_running -eq $true
}
Check '16. close 之后的终端真的收掉了' {
    $r19.payload.effect -eq 'confirmed' -and $r19.payload.killed_running -eq $false
}
Check '17. 终端的 manifest 说清了"我占了什么"和"我做不到什么"' {
    $pres = $script:all | ForEach-Object { $_ | ConvertFrom-Json } | Where-Object { $_.type -eq 'presence' -and $_.payload.manifest } | Select-Object -First 1
    $m = $pres.payload.manifest
    ($m.owns.domain -eq 'shell') -and $m.not_implemented.send -and $m.not_implemented.screen
}
Check '18. 中控照常记账：每一次 run 都有结果收尾' {
    $logPath = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl'
    $records = Get-Content -LiteralPath $logPath | ForEach-Object { $_ | ConvertFrom-Json }
    $runs = @($records | Where-Object { $_.type -eq 'request' -and $_.payload.tool -eq 'run' })
    $chunks = @($records | Where-Object { $_.type -eq 'chunk' })
    $runCorrs = @($runs | ForEach-Object { $_.corr })
    $settled = @($records | Where-Object { $_.type -eq 'result' -and $runCorrs -contains $_.corr })
    ($runs.Count -eq 11) -and ($settled.Count -eq $runCorrs.Count) -and ($chunks.Count -ge 2)
}
<#
  第 19 条是 **2026-09-20 审计**补的，盯的是"服务自己的结果没合它自己声明的形"。

  当时终端 `run` 的成功分支写着 `"why": if trimmed { Some(..) } else { None }`，
  而它声明的 `why` 是 `string`（闭合 schema）—— 于是**每一条输出没被裁剪的普通命令**
  回来都带 `"why": null`，投递闸门每次都得贴一张 `schema_violation`。

  这一条同时钉住两件事：结果**形状合法**，而且闸门**一次都没响**。
#>
Check '19. **没有一条 run 的结果违反终端自己声明的形状**（闸门一次都没响）' {
    $logPath = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl'
    $records = Get-Content -LiteralPath $logPath | ForEach-Object { $_ | ConvertFrom-Json }
    $tripped = @($records | Where-Object { $_.payload.schema_violation -or $_.payload.code -eq 'schema_violation' })
    $nullWhy = @($records | Where-Object { $_.type -eq 'result' -and $_.payload.PSObject.Properties.Name -contains 'why' -and $null -eq $_.payload.why })
    ($tripped.Count -eq 0) -and ($nullWhy.Count -eq 0)
}
Write-Host '== stdout（截断显示）==' -ForegroundColor Cyan
$script:all | ForEach-Object {
    $m = $_ | ConvertFrom-Json
    $code = if ($m.payload.code) { $m.payload.code } else { '' }
    Write-Host ("{0,-5} {1,-8} {2,-7} -> {3,-8} {4}" -f $m.corr, $m.type, $m.from, $m.to, $code)
}
Write-Host '== stderr ==' -ForegroundColor Cyan
foreach ($l in ($script:errText -split "`n")) { if ($l.Trim() -ne '') { Write-Host $l } }
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "工作目录留在：$work"
