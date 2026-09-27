# **长任务：真把压缩撞出来，然后再干一件活**（2026-09-20）。
#
#   pwsh -File scripts/smoke-e2e-long.ps1
#
# 前面几条整机验收都是"一个回合之内"的事。这一条要的是**回合之间的那一层**：
# 把窗口开得很小（`--model-window 6000`），连着下几条任务、每条都读一份不小的文件，
# 逼着线性记忆**真的压缩一次**；然后**再让它干一件真活**。
#
# 要证的三件事：
#   1. 压缩**真的发生了**（不是"我觉得它该发生"）；
#   2. 压缩之后**前缀没有被重编**（§15.2：变化走 append-only）；
#   3. 压缩之后它**还能把活干成**——这一条最要紧：一个"记得住但干不了"的记忆层
#      等于没有。
#
# 顺带**记录**（不判红）一件事：压缩之后它还记不记得前面那些细节 ——
# 那是摘要器的活儿，不该由这条脚本替它打分。
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
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-e2e-long-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null
# 一份**不小**的文件：120 行，够把 6000 的窗口顶穿
$lines = 1..120 | ForEach-Object { "第 $_ 行：这是一份用来把上下文顶穿的文件，MARKER 在最后一行。" }
$lines += 'MARKER=old'
Set-Content -LiteralPath (Join-Path $project 'big.txt') -Value ($lines -join "`n") -Encoding utf8
$sid = 's-long'
$dataDir = Join-Path $work 'data'
$servicesPath = Join-Path $work 'services.json'
@"
[
  { "kind": "editor", "id": "editor:1", "role": "tool",
    "cmd": ["$(J $editor)"], "args": ["--fence", "$(J $project)"] },
  { "kind": "agent", "id": "agent:dada", "role": "agent", "bindings": ["editor:1"],
    "cmd": ["$(J $agentBin)"],
    "args": ["--model", "deepseek", "--base-url", "$BaseUrl", "--model-name", "$ModelName",
             "--persona", "达达", "--fence", "$(J $project)",
             "--model-window", "6000", "--max-steps", "10", "--max-tokens", "800"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ctrl
$psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 60000 --host `"$project`""
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
function WaitCorr([string]$corr, [int]$timeoutMs = 240000) {
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

# 四轮"吃东西"的任务：每一轮都让 `read {path}` 把那一百多行灌进上下文
$answers = @()
for ($i = 1; $i -le 4; $i++) {
    $task = if ($i -eq 1) {
        '请用 read {path:"big.txt"} 读一遍那份文件，然后用一句话说它大概是什么（**不要**改任何东西）。'
    } else {
        '再读一遍 big.txt（read {path:"big.txt"}），然后用一句话复述它的第一行。不要改任何东西。'
    }
    Write-Host ("== 第 $i 轮：往上下文里灌东西 ==") -ForegroundColor Cyan
    Send 'agent:dada' "t$i" @{ task = $task }
    $a = WaitCorr "t$i"
    if ($a) { Write-Host ('  它说：' + ([string]$a.payload.text).Substring(0, [Math]::Min(90, ([string]$a.payload.text).Length))) -ForegroundColor DarkGray }
    $answers += $a
}

# **最后一轮**：压缩（大概）已经发生过，来一件**真活**
$taskReal = '现在把 big.txt 最后那行 MARKER=old 改成 MARKER=done（用编辑器 open + edit），做完告诉我。'
Write-Host '== 最后一轮：压缩之后干一件真活 ==' -ForegroundColor Cyan
Send 'agent:dada' 'tz' @{ task = $taskReal }
$final = WaitCorr 'tz'
if ($final) { Write-Host ('  它说：' + [string]$final.payload.text) -ForegroundColor DarkGray }

$proc.StandardInput.Close()
$proc.WaitForExit(30000) | Out-Null
$drain = $proc.StandardOutput.ReadToEndAsync(); $drain.Wait(5000) | Out-Null
$et = $proc.StandardError.ReadToEndAsync(); $et.Wait(5000) | Out-Null
$errText = if ($et.IsCompleted) { $et.Result } else { '' }
$logPath = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl'
$records = @()
if (Test-Path $logPath) {
    $records = @(Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json })
}
$compactions = @($records | Where-Object { $_.payload.kind -eq 'compaction' })
$promptCompiled = @($records | Where-Object { $_.payload.kind -eq 'prompt_compiled' })
$disk = Get-Content -LiteralPath (Join-Path $project 'big.txt') -Raw
Write-Host '== 账 ==' -ForegroundColor Cyan
Write-Host ("  压缩事件：" + $compactions.Count + " 条；prompt_compiled：" + $promptCompiled.Count + " 条") -ForegroundColor DarkGray
foreach ($c in $compactions) {
    Write-Host ('  压缩：' + ($c.payload | ConvertTo-Json -Compress -Depth 4)) -ForegroundColor DarkGray
}
Check '1. **压缩真的发生了**（窗口 6000 + 四轮读全文）' {
    $compactions.Count -ge 1
}
Check '2. **前缀没有被重编**：`prompt_compiled` 全程只有一条（§15.2）' {
    $promptCompiled.Count -eq 1
}
Check '3. 压缩之后**它还能把活干成**：磁盘上 MARKER=done' {
    $disk -like '*MARKER=done*'
}
Check '4. 每一轮都收尾了（没有一条卡死或 400）' {
    @($answers | Where-Object { $null -eq $_ -or $_.payload.effect -ne 'confirmed' }).Count -eq 0 -and `
        $null -ne $final -and $final.payload.effect -eq 'confirmed'
}
Check '5. 全程没有 `service_down` / `schema_violation`' {
    @($records | Where-Object { $_.payload.code -eq 'service_down' -or $_.payload.schema_violation }).Count -eq 0
}
# **只记录不判分**：压缩之后它还记不记得前面那些细节（那是摘要器的活儿）
$remembered = $false
if ($final) {
    $t = [string]$final.payload.text
    $remembered = $t.Contains('第 1 行') -or $t.Contains('big.txt')
}
Write-Host ("  压缩之后还提不提得到前面那份文件（只记录）：$remembered") -ForegroundColor DarkGray
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    Write-Host '== agent stderr（尾巴）==' -ForegroundColor DarkGray
    ($errText -split "`n") | Select-Object -Last 12 | ForEach-Object { if ($_.Trim() -ne '') { Write-Host ("  " + $_) -ForegroundColor DarkGray } }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
