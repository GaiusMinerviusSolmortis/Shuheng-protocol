# **整机跑一件真活**：一个智能体 + 编辑器 + 终端（2026-09-20）。
#
#   pwsh -File scripts/smoke-e2e-agent.ps1
#
# 这条回答的是那个最实际的问题：**"我拉起一个带编辑器和 shell 的智能体，
# 给它下一条令，它能不能自己把活干完？"**
#
# 前面那些脚本测的都是**零件**（协议、路由、锁、契约、互斥……），
# 这一条测的是**零件合起来是不是一台能用的机器** —— 中间任何一处接不牢
# （名片写错、能力没签、路径没托管、回合循环漏一步），它都干不完。
#
# 没有 API key 就**跳过**（退出码 0）—— 它测的是模型那一半，不是系统那一半。
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
$editor = Join-Path $root 'target\debug\harness-editor.exe'
$term = Join-Path $root 'target\debug\harness-term.exe'
$agentBin = Join-Path $root 'target\debug\harness-agent.exe'
foreach ($b in @($ctrl, $editor, $term, $agentBin)) { if (-not (Test-Path $b)) { throw "产物不在：$b" } }
function J([string]$p) { return ($p -replace '\\', '\\') }

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-e2e-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null
# 一件**真的有事可做**的项目：一个待改的文件 + 一个用来列目录的文件
Set-Content -LiteralPath (Join-Path $project 'app.txt') -Value "name=demo`nversion=1`n" -Encoding utf8 -NoNewline
Set-Content -LiteralPath (Join-Path $project 'readme.md') -Value '# 演示项目' -Encoding utf8

$sid = 's-e2e'
$dataDir = Join-Path $work 'data'
$servicesPath = Join-Path $work 'services.json'
@"
[
  { "kind": "editor", "id": "editor:1", "role": "tool",
    "cmd": ["$(J $editor)"], "args": ["--fence", "$(J $project)"] },
  { "kind": "term", "id": "term:1", "role": "tool",
    "cmd": ["$(J $term)"], "args": ["--fence", "$(J $project)"] },
  { "kind": "agent", "id": "agent:dada", "role": "agent", "bindings": ["editor:1", "term:1"],
    "cmd": ["$(J $agentBin)"],
    "args": ["--model", "deepseek", "--base-url", "$BaseUrl", "--model-name", "$ModelName",
             "--persona", "达达", "--fence", "$(J $project)",
             "--model-window", "32000", "--max-steps", "12", "--max-tokens", "1200"] }
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
$script:all = New-Object System.Collections.Generic.List[string]
$script:replies = @{}
function NextLine([int]$timeoutMs) {
    if ($null -eq $script:outTask) { $script:outTask = $proc.StandardOutput.ReadLineAsync() }
    if (-not $script:outTask.Wait($timeoutMs)) { return $null }
    $line = $script:outTask.Result
    $script:outTask = $null
    return $line
}

# **任务**：故意要**两个工具都用上**，而且要求落到磁盘上可核对。
# 为什么在任务里点名"用编辑器改"：不然模型完全可以用终端 `echo >` 改文件干完活 ——
# 那也是一条合法的路，但那条路测不到编辑器那一半。
$task = '托管目录里有个 app.txt，请用**编辑器**（open + edit）把里面的 `version=1` 改成 `version=2`；' +
        '改完之后用**终端**跑一条命令把 app.txt 的内容打出来，确认改成功了；最后用一句话告诉我结果。'
$msg = [ordered]@{
    id = 'm-1'; ts = 0; type = 'request'; from = 'console'; to = 'agent:dada'
    corr = 'c-1'; sid = $sid; authority = 'user'; payload = @{ task = $task }
}
Write-Host "== 任务 ==" -ForegroundColor Cyan
Write-Host "  $task"
$proc.StandardInput.WriteLine(($msg | ConvertTo-Json -Compress -Depth 12))
$proc.StandardInput.Flush()
$deadline = [DateTime]::UtcNow.AddSeconds(180)
$answer = $null
while ([DateTime]::UtcNow -lt $deadline) {
    $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
    $line = NextLine $left
    if ($null -eq $line) { continue }
    if ($line.Trim() -eq '') { continue }
    $script:all.Add($line) | Out-Null
    $m = $line | ConvertFrom-Json
    if ($m.type -eq 'result') { $script:replies[$m.corr] = $m }
    if ($m.type -eq 'result' -and $m.corr -eq 'c-1') { $answer = $m; break }
}
$proc.StandardInput.Close()
$proc.WaitForExit(15000) | Out-Null
$drain = $proc.StandardOutput.ReadToEndAsync(); $drain.Wait(5000) | Out-Null
if ($drain.IsCompleted -and $drain.Result) {
    foreach ($l in ($drain.Result -split "`n")) { if ($l.Trim() -ne '') { $script:all.Add($l.Trim()) | Out-Null } }
}
$et = $proc.StandardError.ReadToEndAsync(); $et.Wait(5000) | Out-Null
$logPath = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl'
$records = @()
if (Test-Path $logPath) {
    $records = @(Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json })
}

$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}

$calls = @($records | Where-Object { $_.type -eq 'request' -and $_.from -eq 'agent:dada' -and $_.payload.tool })
$toEditor = @($calls | Where-Object { $_.to -eq 'editor:1' })
$toTerm = @($calls | Where-Object { $_.to -eq 'term:1' })
$final = Get-Content -LiteralPath (Join-Path $project 'app.txt') -Raw

Write-Host '== 它干了什么 ==' -ForegroundColor Cyan
foreach ($c in $calls) {
    $args = ($c.payload.args | ConvertTo-Json -Compress -Depth 6)
    if ($args.Length -gt 110) { $args = $args.Substring(0, 110) + '…' }
    Write-Host ('  → ' + $c.to + ' "' + $c.payload.tool + '" ' + $args) -ForegroundColor DarkGray
}
if ($answer) { Write-Host ("  结论：" + $answer.payload.text) -ForegroundColor DarkGray }

Check '1. 模型给出了结论（不是"模型这条路断了"）' {
    $null -ne $answer -and $answer.payload.effect -eq 'confirmed' -and $answer.payload.text.Length -gt 0
}
Check '2. 它**真的动了手**（不是空口回答）' {
    $calls.Count -ge 1
}
Check '3. 两个工具**都用上了**：编辑器改、终端验（这是任务点名的）' {
    $toEditor.Count -ge 1 -and $toTerm.Count -ge 1
}
Check '4. **活真干成了**：磁盘上 app.txt 变成 version=2' {
    $final -like '*version=2*' -and $final -notlike '*version=1*'
}
Check '5. **环闭上了**：工具的结果回到了智能体手里（它才有东西可总结）' {
    @($records | Where-Object { $_.type -eq 'result' -and $_.to -eq 'agent:dada' -and $_.from -ne 'ctrl' }).Count -ge 2
}
Check '6. 它**没说谎**：结论里提到 version=2' {
    $answer -and ($answer.payload.text -like '*version=2*' -or $answer.payload.text -like '*version 2*')
}
Check '7. 全程没有 `service_down` / `schema_violation`（没有靠"坏掉的东西"蒙过去）' {
    @($records | Where-Object { $_.payload.code -eq 'service_down' -or $_.payload.schema_violation }).Count -eq 0
}

Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    Write-Host '== 会话日志（最后 20 条）==' -ForegroundColor DarkGray
    $records | Select-Object -Last 20 | ForEach-Object {
        Write-Host ("  " + $_.seq + "  " + $_.type + " " + $_.from + "→" + $_.to + "  " + $_.payload.code) -ForegroundColor DarkGray
    }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host ("项目留在：" + $project) -ForegroundColor DarkGray
