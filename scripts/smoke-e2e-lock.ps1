# **两个智能体抢同一份文件**（2026-09-20）。
#
#   pwsh -File scripts/smoke-e2e-lock.ps1
#
# 前面三条整机验收都是**一个**智能体。这一条把**两个**放进同一个会话、
# 绑同一个编辑器实例，让它们抢同一份文件。
#
# 要证的是一条设计上的硬话（§5.15）：**实例是锁点，同一时刻只有一个持有者。**
# 零件层面 `smoke-editor` 早验过（两个实例、同一个路径 ⇒ busy）；
# 这条验的是**合起来**：两个**真模型**各自决定动手时，锁还拦不拦得住，
# 以及**被挡下的那个会不会老实说**。
#
# 没有 API key 就**跳过**（退出码 0）。
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
    exit 0
}
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$bin = Join-Path $root 'target\debug'
$ctrl = Join-Path $bin 'harness-ctrl.exe'
$editor = Join-Path $bin 'harness-editor.exe'
$agentBin = Join-Path $bin 'harness-agent.exe'
foreach ($b in @($ctrl, $editor, $agentBin)) { if (-not (Test-Path $b)) { throw "产物不在：$b" } }
function J([string]$p) { return ($p -replace '\\', '\\') }
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-e2e-lock-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null
Set-Content -LiteralPath (Join-Path $project 'app.txt') -Value "name=demo`nversion=1`n" -Encoding utf8 -NoNewline
$sid = 's-lock'
$dataDir = Join-Path $work 'data'
$servicesPath = Join-Path $work 'services.json'
@"
[
  { "kind": "editor", "id": "editor:1", "role": "tool",
    "cmd": ["$(J $editor)"], "args": ["--fence", "$(J $project)"] },
  { "kind": "agent", "id": "agent:alice", "role": "agent", "bindings": ["editor:1"],
    "cmd": ["$(J $agentBin)"],
    "args": ["--model", "deepseek", "--base-url", "$BaseUrl", "--model-name", "$ModelName",
             "--persona", "爱丽丝", "--fence", "$(J $project)",
             "--model-window", "32000", "--max-steps", "8", "--max-tokens", "1000"] },
  { "kind": "agent", "id": "agent:bob", "role": "agent", "bindings": ["editor:1"],
    "cmd": ["$(J $agentBin)"],
    "args": ["--model", "deepseek", "--base-url", "$BaseUrl", "--model-name", "$ModelName",
             "--persona", "鲍勃", "--fence", "$(J $project)",
             "--model-window", "32000", "--max-steps", "8", "--max-tokens", "1000"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ctrl
$psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 30000 --host `"$project`""
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
$psi.EnvironmentVariables['DEEPSEEK_API_KEY'] = $key
$proc = [System.Diagnostics.Process]::Start($psi)
$script:outTask = $null
function NextLine([int]$timeoutMs) {
    if ($null -eq $script:outTask) { $script:outTask = $proc.StandardOutput.ReadLineAsync() }
    if (-not $script:outTask.Wait($timeoutMs)) { return $null }
    $line = $script:outTask.Result
    $script:outTask = $null
    return $line
}
function Send([string]$to, [string]$corr, [hashtable]$payload) {
    $m = [ordered]@{ id = "m-$corr"; ts = 0; type = 'request'; from = 'console'
                     to = $to; corr = $corr; sid = $script:sid; authority = 'user'; payload = $payload }
    $proc.StandardInput.WriteLine(($m | ConvertTo-Json -Compress -Depth 12))
    $proc.StandardInput.Flush()
}
function WaitCorr([string]$corr, [int]$timeoutMs = 180000) {
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
Start-Sleep -Seconds 2
Send 'ctrl' 'r0' @{ op = 'list' }
$null = WaitCorr 'r0' 20000
# **爱丽丝**：改文件，而且**要求她别松手**（这条测试要的是"锁还在"）
$taskA = '请打开 app.txt（用编辑器），把 version=1 改成 version=2。改完之后**不要 close** —— 我要那份写权一直拿着，后面还有用。'
Write-Host '== 爱丽丝：改文件并**继续持有** ==' -ForegroundColor Cyan
Send 'agent:alice' 'a1' @{ task = $taskA }
$ansA = WaitCorr 'a1'
if ($ansA) { Write-Host ('  爱丽丝：' + $ansA.payload.text) -ForegroundColor DarkGray }
# **鲍勃**：同一个文件，说明白"先试着打开"
$taskB = '请**先试着**用编辑器打开 app.txt（我知道它可能被别人占着），把结果照实告诉我 —— 能打开就改成 version=3，打不开就说清为什么。'
Write-Host '== 鲍勃：抢同一份文件 ==' -ForegroundColor Cyan
Send 'agent:bob' 'b1' @{ task = $taskB }
$ansB = WaitCorr 'b1'
if ($ansB) { Write-Host ('  鲍勃：' + $ansB.payload.text) -ForegroundColor DarkGray }
$proc.StandardInput.Close()
$proc.WaitForExit(20000) | Out-Null
$drain = $proc.StandardOutput.ReadToEndAsync(); $drain.Wait(5000) | Out-Null
$et = $proc.StandardError.ReadToEndAsync(); $et.Wait(5000) | Out-Null
$logPath = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl'
$records = @()
if (Test-Path $logPath) {
    $records = @(Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json })
}
$aliceCalls = @($records | Where-Object { $_.type -eq 'request' -and $_.from -eq 'agent:alice' -and $_.payload.tool })
$bobCalls = @($records | Where-Object { $_.type -eq 'request' -and $_.from -eq 'agent:bob' -and $_.payload.tool })
$bobResults = @($records | Where-Object { $_.type -eq 'result' -and $_.to -eq 'agent:bob' -and $_.from -eq 'editor:1' })
$final = Get-Content -LiteralPath (Join-Path $project 'app.txt') -Raw
Write-Host '== 两个人的调用 ==' -ForegroundColor Cyan
Write-Host ('  爱丽丝：' + (($aliceCalls | ForEach-Object { $_.payload.tool }) -join ' -> ')) -ForegroundColor DarkGray
Write-Host ('  鲍勃：  ' + (($bobCalls | ForEach-Object { $_.payload.tool }) -join ' -> ')) -ForegroundColor DarkGray
if ($bobResults.Count -gt 0 -and $bobResults[0].payload.why) {
    Write-Host ('  鲍勃收到的拒绝：' + $bobResults[0].payload.why) -ForegroundColor DarkGray
}
Check '1. 爱丽丝拿到了写权（她的 open 成功了）' {
    (@($aliceCalls | Where-Object { $_.payload.tool -eq 'open' }).Count -ge 1) -and `
        (@($records | Where-Object { $_.type -eq 'result' -and $_.to -eq 'agent:alice' -and $_.payload.instance_id -eq 'ed-1' }).Count -ge 1)
}
Check '2. 爱丽丝的改动落了盘（version=2）' {
    $final -like '*version=2*'
}
Check '3. 鲍勃**动不了**：文件里没有他那个 version=3，而且他收到了一条拒绝' {
    # 钉的是**结果**（文件里的值是爱丽丝写的），不是"他撞没撞" ——
    # 真模型可能先看一眼 status 就明白了（那是更聪明的走法）。
    ($final -notlike '*version=3*') -and (@($bobResults | Where-Object { $_.payload.code -eq 'refused' -or $_.payload.code -eq 'busy' }).Count -ge 1)
}
Check '4. 被挡下时**说清是谁占着**（判词里提到写权/持有者）' {
    $why = if ($bobResults.Count -gt 0) { [string]$bobResults[0].payload.why } else { '' }
    $why -like '*写权*' -or $why -like '*held_by*' -or $why -like '*已经有*'
}
Check '5. 鲍勃**没有撒谎**（结论里说了没改成 / 打不开）' {
    $t = if ($ansB) { [string]$ansB.payload.text } else { '' }
    $limiting = @('打不开', '改不了', '不能', '没有', '占', '被持有', 'busy', 'held', 'already',
                  "can't", 'cannot', 'unable', 'not able', 'refused', 'denied')
    @($limiting | Where-Object { $t -like "*$_*" }).Count -ge 1
}
Check '6. 两个人**都在**（一人被挡不等于另一个人被打断）' {
    $null -ne $ansA -and $ansA.payload.effect -eq 'confirmed' -and `
        $null -ne $ansB -and $ansB.payload.effect -eq 'confirmed'
}
Check '7. 全程没有 `service_down` / `schema_violation`' {
    @($records | Where-Object { $_.payload.code -eq 'service_down' -or $_.payload.schema_violation }).Count -eq 0
}
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
