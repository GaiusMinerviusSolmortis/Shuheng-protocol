# **崩在半路：半成品和对话保不保得住**（2026-09-20）。
#
#   pwsh -File scripts/smoke-e2e-crashresume.ps1
#
# 前面那条 `smoke-resume` 验的是**干净收摊**之后能不能冷启动（mock 模型）。
# 这一条问的是更常见、也更难的那个：**干到一半被硬杀**（`Stop-Process`，
# 不给它收摊的机会）—— 那时候：
#
#   1. **已经改了一半的东西**还在不在？（它在用户自己的文件里，不在我们的日志里）
#   2. **用户的对话**还在不在？（在日志里）
#   3. 冷启动之后**还能不能接着干活**？
#
# 判"对话保住了"用的是一个**只有对话里才有**的东西：第一条任务里埋的**口令**。
# 磁盘上没有它、服务表里没有它 —— 答得出来，就说明上下文真的是从日志重建的，
# 而不是它现场猜的。
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
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-e2e-cr-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null
Set-Content -LiteralPath (Join-Path $project 'app.txt') -Value "name=demo`nversion=1`n" -Encoding utf8 -NoNewline
Set-Content -LiteralPath (Join-Path $project 'note.md') -Value "待办" -Encoding utf8 -NoNewline
$sid = 's-cr'
$dataDir = Join-Path $work 'data'
$servicesPath = Join-Path $work 'services.json'
function Write-Services([string]$path, [bool]$Resume) {
    # ⚠️ **变量名别取 `$resume`**：PowerShell 的变量名**不区分大小写**，
    # 它就是参数 `$Resume` 本身 —— 于是插进 JSON 的是布尔值 `False`，
    # 服务表当场坏掉（`agent:dada` 根本没起起来，任务全撞 `instance_gone`）。
    # 第一版就是这么错的。
    $resumeArg = if ($Resume) { ', "--resume"' } else { '' }
    @"
[
  { "kind": "editor", "id": "editor:1", "role": "tool",
    "cmd": ["$(J $editor)"], "args": ["--fence", "$(J $project)"] },
  { "kind": "agent", "id": "agent:dada", "role": "agent", "bindings": ["editor:1"],
    "cmd": ["$(J $agentBin)"],
    "args": ["--model", "deepseek", "--base-url", "$BaseUrl", "--model-name", "$ModelName",
             "--persona", "达达", "--fence", "$(J $project)",
             "--model-window", "32000", "--max-steps", "10", "--max-tokens", "1000"$resumeArg] }
]
"@ | Set-Content -LiteralPath $path -Encoding utf8
}
Write-Services $servicesPath $false
function Start-Ctrl([string]$svcPath) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $ctrl
    $psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$svcPath`" --drain-ms 30000"
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
    $psi.EnvironmentVariables['DEEPSEEK_API_KEY'] = $key
    return [System.Diagnostics.Process]::Start($psi)
}
$script:outTask = $null
function NextLine([int]$timeoutMs) {
    if ($null -eq $script:outTask) { $script:outTask = $script:proc.StandardOutput.ReadLineAsync() }
    if (-not $script:outTask.Wait($timeoutMs)) { return $null }
    $line = $script:outTask.Result
    $script:outTask = $null
    return $line
}
function Send([string]$to, [string]$corr, [hashtable]$payload) {
    $m = [ordered]@{ id = "m-$corr"; ts = 0; type = 'request'; from = 'console'
                     to = $to; corr = $corr; sid = $script:sid; authority = 'user'; payload = $payload }
    $script:proc.StandardInput.WriteLine(($m | ConvertTo-Json -Compress -Depth 12))
    $script:proc.StandardInput.Flush()
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
$logPath = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl'

# ---- 第一程：下一道**多步**任务，做到一半硬杀 ----
$password = '雨燕-7391'   # **只在对话里出现过的暗号**（磁盘上没有、服务表里也没有）
$script:proc = Start-Ctrl $servicesPath
Start-Sleep -Seconds 2
Send 'ctrl' 'r0' @{ op = 'list' }
$null = WaitCorr 'r0' 20000
$task1 = "请做两件事：① 用编辑器把 app.txt 里的 version=1 改成 version=2；" +
         "② 然后用编辑器读一遍 app.txt 确认。做完用一句话告诉我。" +
         "另外记一下：这次的口令是 $password（后面我会问你）。"
Write-Host '== 第一程：下多步任务，并在它改完之后硬杀 ==' -ForegroundColor Cyan
Send 'agent:dada' 't1' @{ task = $task1 }
$killed = $false
$answer1 = $null
$deadline = [DateTime]::UtcNow.AddSeconds(180)
while ([DateTime]::UtcNow -lt $deadline) {
    $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
    $line = NextLine ([int][Math]::Min($left, 1200))
    if ($line -and $line.Trim() -ne '') {
        $m = $line | ConvertFrom-Json
        if ($m.type -eq 'result' -and $m.corr -eq 't1') { $answer1 = $m; break }
    }
    if (-not $killed -and (Test-Path $logPath)) {
        # 它把 app.txt 改完了吗？（日志里出现"编辑器的结果"就算）
        $hit = @(Get-Content -LiteralPath $logPath -ErrorAction SilentlyContinue |
            Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json } |
            Where-Object { $_.type -eq 'result' -and $_.from -eq 'editor:1' -and $_.payload.changed })
        if ($hit.Count -ge 1) {
            Write-Host '  → 它已经改完了，我这就**硬杀整个中控**（不给它收摊的机会）' -ForegroundColor Yellow
            $script:proc.Kill()
            $killed = $true
            break
        }
    }
}
Start-Sleep -Seconds 1
$diskAfterCrash = Get-Content -LiteralPath (Join-Path $project 'app.txt') -Raw
Check '0. 前提：我**在它干到一半时硬杀了整个会话**（不是等它收尾）' {
    $killed -and $null -eq $answer1
}
Check '1. **半成品留住了**：磁盘上 version 已经是 2（它落在用户自己的文件里）' {
    $diskAfterCrash -like '*version=2*'
}

# ---- 第二程：同一个会话冷启动（智能体带 --resume） ----
Write-Host '== 第二程：同 sid 冷启动，问它上一程的事 ==' -ForegroundColor Cyan
$services2 = Join-Path $work 'services-resume.json'
Write-Services $services2 $true
$script:proc = Start-Ctrl $services2
Start-Sleep -Seconds 3
Send 'ctrl' 'r1' @{ op = 'list' }
$null = WaitCorr 'r1' 25000
$task2 = '我们刚被打断了。请回答两件事：① 我刚才给你的口令是什么？② app.txt 现在是什么状态？'
Send 'agent:dada' 't2' @{ task = $task2 }
$answer2 = WaitCorr 't2'
if ($answer2) { Write-Host ('  它说：' + [string]$answer2.payload.text) -ForegroundColor DarkGray }
# ---- 第三程：还能接着干活吗 ----
$task3 = '现在把 note.md 里的「待办」改成「已完成」（用编辑器 open + edit + close）。'
Send 'agent:dada' 't3' @{ task = $task3 }
$answer3 = WaitCorr 't3'
if ($answer3) { Write-Host ('  它说：' + [string]$answer3.payload.text) -ForegroundColor DarkGray }
$script:proc.StandardInput.Close()
$script:proc.WaitForExit(30000) | Out-Null
$drain = $script:proc.StandardOutput.ReadToEndAsync(); $drain.Wait(5000) | Out-Null
$et = $script:proc.StandardError.ReadToEndAsync(); $et.Wait(5000) | Out-Null
$records = @()
if (Test-Path $logPath) {
    $records = @(Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json })
}
$diskNote = Get-Content -LiteralPath (Join-Path $project 'note.md') -Raw
Check '2. 冷启动之后**会话还在**（日志是同一个文件、只追加，没被截断）' {
    # 老账还在 + 新账也进来了
    (@($records | Where-Object { $_.type -eq 'request' -and $_.payload.tool -eq 'edit' }).Count -ge 1) -and `
        (@($records | Where-Object { $_.corr -eq 't2' }).Count -ge 1)
}
Check '3. **用户的对话也留住了**：它答得出那个只在对话里出现过的口令' {
    $answer2 -and ([string]$answer2.payload.text).Contains($password)
}
Check '4. 它知道文件现在的状态（version=2）' {
    $answer2 -and ([string]$answer2.payload.text) -like '*version=2*'
}
Check '5. **恢复之后还能接着干活**：note.md 被改成了「已完成」' {
    $diskNote -like '*已完成*'
}
Check '6. 半成品**从头到尾没丢**（app.txt 还是 version=2）' {
    (Get-Content -LiteralPath (Join-Path $project 'app.txt') -Raw) -like '*version=2*'
}
Check '7. 恢复时**补了一条敕令**：说清"上一辈子的实例都没了"（别让模型拿着旧 id 撞墙）' {
    # 这条钉的是 2026-09-20 抓到的一处"恢复之后的认知偏差"：上下文里有
    # "我 open 了 ed-1"，却没有"它后来没了"——那条敕令当时发给了**上一个进程**。
    # 于是新生的它张口就说"我仍持有 ed-1 的写权"（原话，而实例早没了）。
    $inj = @($records | Where-Object {
        $_.payload.kind -eq 'model_item' -and
        $_.payload.item.kind -eq 'injected' -and
        ([string]$_.payload.item.text) -like '*上一次会话*'
    })
    $inj.Count -ge 1
}
# **只记录不判分**：它答话时还会不会声称自己拿着旧实例（判分该由"它有没有被纠正"来判）
$staleClaim = $false
if ($answer2) { $staleClaim = ([string]$answer2.payload.text) -like '*仍持有*' }
Write-Host ("  恢复后它还提不提'仍持有'（只记录）：$staleClaim") -ForegroundColor DarkGray
Write-Host ''
Write-Host ('  日志里一共有 ' + $records.Count + ' 条；硬杀那一下之后，账是从头继续记的') -ForegroundColor DarkGray
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
