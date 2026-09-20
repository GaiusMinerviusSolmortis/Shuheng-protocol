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
$psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 20000"
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
