# 阶段 4b 的验收：**接真实模型**（DeepSeek）。
#
#   pwsh -File scripts/smoke-deepseek.ps1
#
# 这一半测的是 mock 根本测不到的：**模型能不能正确调用工具**
# —— 也就是工具名片写清楚了没有、循环能不能让它把活干完（README 阶段 4b）。
#
# 没有 API key 就**跳过**（退出码 0），这样整套验收在别的机器上也跑得动。
# key 的来源：环境变量 `DEEPSEEK_API_KEY`，或 `--env-file`（默认 C:\...\WXBot\.env）。
param(
    [string]$EnvFile = 'D:\WXBot\.env',
    [string]$BaseUrl = 'https://api.deepseek.com',
    [string]$ModelName = 'deepseek-chat'
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
function Get-Key() {
    if ($env:DEEPSEEK_API_KEY) { return $env:DEEPSEEK_API_KEY }
    if (Test-Path $EnvFile) {
        foreach ($line in Get-Content -LiteralPath $EnvFile) {
            $l = $line.Trim()
            if ($l.StartsWith('#') -or -not $l.Contains('=')) { continue }
            $kv = $l.Split('=', 2)
            if ($kv[0].Trim() -eq 'DEEPSEEK_API_KEY') {
                $v = $kv[1].Trim().Trim('"').Trim("'")
                if ($v) { return $v }
            }
        }
    }
    return $null
}
$key = Get-Key
if (-not $key) {
    Write-Host "跳过：没有拿到 DEEPSEEK_API_KEY（环境变量里没有，$EnvFile 里也没有）。" -ForegroundColor Yellow
    Write-Host '这一步是可选的：它测的是模型，不是系统。'
    exit 0
}
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$term = Join-Path $root 'target\debug\harness-term.exe'
$echo = Join-Path $root 'target\debug\harness-echo.exe'
$agentBin = Join-Path $root 'target\debug\harness-agent.exe'
function J([string]$p) { return ($p -replace '\\', '\\') }
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-ds-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null
Set-Content -LiteralPath (Join-Path $project 'hello.txt') -Value 'hi' -Encoding utf8
Set-Content -LiteralPath (Join-Path $project 'notes.md') -Value '# 笔记' -Encoding utf8
$sid = 's-ds'
$dataDir = Join-Path $work 'data'
# **参数表不再由测试脚本手写** —— 它来自终端服务自己的 manifest（§11.1 C 组），
# 中控转给智能体，智能体再交给 provider。这里曾经有一份手抄的 `cards.json`，
# 既没人读、又把字段名写成了旧版（`parameters`）—— 2026-09-19 删掉。
$servicesPath = Join-Path $work 'services.json'
@"
[
  { "kind": "term", "id": "term:1", "role": "tool",
    "cmd": ["$(J $term)"], "args": ["--fence", "$(J $project)"] },
  { "kind": "agent", "id": "agent:dada", "role": "agent", "bindings": ["term:1"],
    "cmd": ["$(J $agentBin)"],
    "args": ["--model", "deepseek", "--base-url", "$BaseUrl", "--model-name", "$ModelName",
             "--persona", "达达", "--fence", "$(J $project)",
              "--model-window", "16000", "--max-steps", "10", "--max-tokens", "800"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ctrl
$psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 20000 --host `"$project`""
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
$psi.EnvironmentVariables['DEEPSEEK_API_KEY'] = $key
$proc = [System.Diagnostics.Process]::Start($psi)
$script:outTask = $null
$script:all = New-Object System.Collections.Generic.List[string]
$script:replies = @{}
function NextLine([int]$timeoutMs) {
    if ($null -eq $script:outTask) { $script:outTask = $proc.StandardOutput.ReadLineAsync() }
    if (-not $script:outTask.Wait($timeoutMs)) { return $null }
    $line = $script:outTask.Result
    $script:outTask = $null
    return $line
}
$task = '用终端看看托管目录里有哪些文件，然后只用一句话把文件名列给我。'
$msg = [ordered]@{
    id = 'm-1'; ts = 0; type = 'request'; from = 'console'; to = 'agent:dada'
    corr = 'c-1'; sid = $sid; authority = 'user'; payload = @{ task = $task }
}
Write-Host "== 任务：$task ==" -ForegroundColor Cyan
$proc.StandardInput.WriteLine(($msg | ConvertTo-Json -Compress -Depth 12))
$proc.StandardInput.Flush()
$deadline = [DateTime]::UtcNow.AddSeconds(120)
$answer = $null
while ([DateTime]::UtcNow -lt $deadline) {
    $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
    $line = NextLine $left
    if ($null -eq $line) { continue }
    if ($line.Trim() -eq '') { continue }
    $script:all.Add($line)
    $m = $line | ConvertFrom-Json
    if ($m.type -eq 'result') { $script:replies[$m.corr] = $m }
    if ($m.type -eq 'result' -and $m.corr -eq 'c-1') { $answer = $m; break }
}
$proc.StandardInput.Close()
$proc.WaitForExit(10000) | Out-Null
$drain = $proc.StandardOutput.ReadToEndAsync(); $drain.Wait(5000) | Out-Null
if ($drain.IsCompleted -and $drain.Result) { foreach ($l in ($drain.Result -split "`n")) { if ($l.Trim() -ne '') { $script:all.Add($l.Trim()) } } }
$et = $proc.StandardError.ReadToEndAsync(); $et.Wait(5000) | Out-Null
$logPath = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl'
$records = @()
if (Test-Path $logPath) { $records = @(Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json }) }
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
Write-Host '== 判据 ==' -ForegroundColor Cyan
Check '1. 模型给出了结论（不是模型这条路断了）' {
    $answer -and $answer.payload.effect -eq 'confirmed' -and $answer.payload.text.Length -gt 0
}
Check '2. 它**自己决定**调了工具（不是空口回答）' {
    # 只钉"至少调了一次" —— 调什么、调几步，**模型自己说了算**（4b 本来就不可复现）
    $answer -and $answer.payload.trace.tool_calls -ge 1
}
Check '3. 它的调用真的到了工具那边（不是只在中控里空转）' {
    $toService = @($records | Where-Object { $_.type -eq 'request' -and $_.from -eq 'agent:dada' -and $_.to -ne 'ctrl' })
    $toService.Count -ge 1
}
Check '4. **环闭上了**：工具的结果回到了智能体手里' {
    # 这条才是要紧的。至于它跑的是 `dir /b` 还是别的，**不该由判据规定** ——
    # 我第一版把具体命令写进了断言，于是模型换个做法就"失败"，那是判据的错。
    $back = @($records | Where-Object { $_.type -eq 'result' -and $_.to -eq 'agent:dada' -and $_.from -ne 'ctrl' })
    $back.Count -ge 1
}
Check '5. 用的是**真实用量**校准基线（provider 报了 prompt_tokens）' {
    $answer.payload.trace.tokens -gt 0
}
Check '6. 模型看到的那段前缀仍然进了日志（不变量二）' {
    ($records | Where-Object { $_.payload.kind -eq 'prompt_compiled' }) -ne $null
}
Check '7. 整个回合包在步数上限内' {
    $answer.payload.trace.steps -le 10
}
<#
  ---- 第二幕：**用真模型做摘要**（待定 #103，2026-09-20）----

  在这之前压缩用的是 mock 摘要器（确定性填充）—— 对"测系统"是对的，
  对"真的在长会话里干活"是错的：**摘要的质量就是压完之后模型还能不能接着干**。

  怎么把它逼出来：开一个**很小的窗口**（1200），连发几条短任务 ——
  历史一过触发线就会真的压一次，而那次摘要**由真模型写**。
  判据盯三样：谁写的（`summarizer`）、那次调用花了多少 token、摘要正文非空。
#>
Write-Host ''
Write-Host '== 第二幕：小窗口 + 真模型摘要 ==' -ForegroundColor Cyan
$work2 = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-ds2-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$proj2 = Join-Path $work2 'project'
New-Item -ItemType Directory -Path $proj2 -Force | Out-Null
Set-Content -LiteralPath (Join-Path $proj2 'a.txt') -Value 'hi' -Encoding utf8
$sid2 = 's-ds2'
$dataDir2 = Join-Path $work2 'data'
$services2 = Join-Path $work2 'services.json'
@"
[
  { "kind": "term", "id": "term:1", "role": "tool",
    "cmd": ["$(J $term)"], "args": ["--fence", "$(J $proj2)"] },
  { "kind": "agent", "id": "agent:dada", "role": "agent", "bindings": ["term:1"],
    "cmd": ["$(J $agentBin)"],
    "args": ["--model", "deepseek", "--base-url", "$BaseUrl", "--model-name", "$ModelName",
             "--persona", "达达", "--fence", "$(J $proj2)",
             "--model-window", "900", "--max-steps", "4", "--max-tokens", "300",
             "--summarize", "model"] }
]
"@ | Set-Content -LiteralPath $services2 -Encoding utf8
$psi2 = New-Object System.Diagnostics.ProcessStartInfo
$psi2.FileName = $ctrl
$psi2.Arguments = "--sid $sid2 --data-dir `"$dataDir2`" --services `"$services2`" --drain-ms 20000 --host `"$project`""
$psi2.RedirectStandardInput = $true
$psi2.RedirectStandardOutput = $true
$psi2.RedirectStandardError = $true
$psi2.UseShellExecute = $false
$psi2.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
$psi2.EnvironmentVariables['DEEPSEEK_API_KEY'] = $key
$p2 = [System.Diagnostics.Process]::Start($psi2)
$script:outTask2 = $null
function NextLine2([int]$timeoutMs) {
    if ($null -eq $script:outTask2) { $script:outTask2 = $p2.StandardOutput.ReadLineAsync() }
    if (-not $script:outTask2.Wait($timeoutMs)) { return $null }
    $line = $script:outTask2.Result
    $script:outTask2 = $null
    return $line
}
# 连发几条短任务：目标不是考模型，是**把历史顶过触发线**
$done = 0
for ($i = 1; $i -le 6; $i++) {
    $corr = "k-$i"
    $m = [ordered]@{ id = "m-$i"; ts = 0; type = 'request'; from = 'console'; to = 'agent:dada'
                     corr = $corr; sid = $sid2; authority = 'user'
                     # **故意让它多写几句**：这一段的任务不是考模型，是把历史顶过触发线
                     payload = @{ task = "第 $i 件事：用两三句话说说你现在能看见什么工具、你打算怎么用它，然后停下。" } }
    $p2.StandardInput.WriteLine(($m | ConvertTo-Json -Compress -Depth 12))
    $p2.StandardInput.Flush()
    $deadline = [DateTime]::UtcNow.AddSeconds(60)
    while ([DateTime]::UtcNow -lt $deadline) {
        $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
        $line = NextLine2 $left
        if ($null -eq $line) { continue }
        if ($line.Trim() -eq '') { continue }
        $r = $line | ConvertFrom-Json
        if ($r.type -eq 'result' -and $r.corr -eq $corr) { $done++; break }
    }
    # 已经压过一次就够了：剩下的不用再花 token
    $log2 = Join-Path (Join-Path (Join-Path $dataDir2 'sessions') $sid2) 'log.jsonl'
    if ((Test-Path $log2) -and ((Get-Content -LiteralPath $log2 | Select-String -Pattern '"kind":"compaction"').Count -ge 1)) {
        break
    }
}
$p2.StandardInput.Close()
$p2.WaitForExit(15000) | Out-Null
$et2 = $p2.StandardError.ReadToEndAsync(); $et2.Wait(5000) | Out-Null
$logPath2 = Join-Path (Join-Path (Join-Path $dataDir2 'sessions') $sid2) 'log.jsonl'
$records2 = @()
if (Test-Path $logPath2) {
    $records2 = @(Get-Content -LiteralPath $logPath2 | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json })
}
$compactions = @($records2 | Where-Object { $_.payload.kind -eq 'compaction' })

Check '8. **真的压了一次**（小窗口连着几条任务就该到触发线）' {
    $done -ge 1 -and $compactions.Count -ge 1
}
Check '9. **这一刀是模型切的**：`summarizer` 是 `model`，而且那次调用花了 token' {
    $c = $compactions | Select-Object -First 1
    $c -and $c.payload.summarizer -eq 'model' -and $c.payload.summarizer_tokens_in -gt 0
}
Check '10. 摘要**不是空的**，而且它进了日志（不变量二：模型接下来会看到它）' {
    $c = $compactions | Select-Object -First 1
    $c -and $c.payload.summary_chars -gt 20 -and $c.payload.summary -and `
        $c.payload.summary.Length -gt 20
}
Check '11. 没退回确定性那条（退回了要在这里看得见原因）' {
    $c = $compactions | Select-Object -First 1
    $c -and $c.payload.summarizer_fell_back -eq $null
}
Write-Host ''
Write-Host '== 模型说了什么 ==' -ForegroundColor Cyan
Write-Host "  $($answer.payload.text)"
Write-Host ''
Write-Host '== 它的动作序列 ==' -ForegroundColor Cyan
$records | Where-Object { $_.from -eq 'agent:dada' -and $_.type -eq 'request' } | ForEach-Object {
    $a = $_.payload.args | ConvertTo-Json -Compress -Depth 5
    Write-Host "  → $($_.to)  $($_.payload.tool)  $a"
}
Write-Host ''
Write-Host '== stderr（含模型与压缩的痕迹）==' -ForegroundColor Cyan
if ($et.IsCompleted) { foreach ($l in ($et.Result -split "`n")) { if ($l.Trim() -ne '') { Write-Host "  $l" } } }
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "工作目录留在：$work"
