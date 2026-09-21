# **整机：让智能体用一个 MCP 工具**（2026-09-20）。
#
#   pwsh -File scripts/smoke-e2e-mcp.ps1
#
# 前面几条整机验收用的都是**我们自己写的**工具（编辑器）。这一条换成 **MCP 桥**：
# 智能体绑的是 `mcp:1`，而它背后是一个真的 MCP server（`mock-mcp`）。
# 中间隔着三层：**智能体 → 中控 → MCP 桥 → 对端进程**。
#
# 要证的是那句定位（附录 §10.3）：**框架是 MCP 的宿主** ——
# 对智能体来说，MCP 工具和自研工具**没有任何区别**：一样有名片、一样有参数表、
# 一样受能力约束、一样能调。
#
# 顺带压一件事：**白名单**（默认拒绝）在真模型面前也成立 ——
# 不在 `--allow` 里的工具，模型就算叫得出名字也调不动。
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
$bridge = Join-Path $bin 'harness-mcp.exe'
$mock = Join-Path $bin 'mock-mcp.exe'
$agentBin = Join-Path $bin 'harness-agent.exe'
foreach ($b in @($ctrl, $bridge, $mock, $agentBin)) { if (-not (Test-Path $b)) { throw "产物不在：$b" } }
function J([string]$p) { return ($p -replace '\\', '\\') }
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-e2e-mcp-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null
Set-Content -LiteralPath (Join-Path $project 'note.txt') -Value 'hello' -Encoding utf8
$sid = 's-mcp'
$dataDir = Join-Path $work 'data'
$servicesPath = Join-Path $work 'services.json'
# **白名单只放 `echo` 和 `add`** —— `boom` / `crash` / `sleep` 都不许调。
# 这条是"默认拒绝"在真模型面前的现场：模型叫得出名字，也调不动。
@"
[
  { "kind": "mcp", "id": "mcp:1", "role": "tool",
    "cmd": ["$(J $bridge)"],
    "args": ["--server", "$(J $mock)", "--allow", "echo", "--allow", "add"] },
  { "kind": "agent", "id": "agent:dada", "role": "agent", "bindings": ["mcp:1"],
    "cmd": ["$(J $agentBin)"],
    "args": ["--model", "deepseek", "--base-url", "$BaseUrl", "--model-name", "$ModelName",
             "--persona", "达达", "--fence", "$(J $project)",
             "--model-window", "32000", "--max-steps", "10", "--max-tokens", "1000"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ctrl
$psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 30000"
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
Start-Sleep -Seconds 3
Send 'ctrl' 'r0' @{ op = 'list' }
$null = WaitCorr 'r0' 25000
# 任务要**两次调用**：一次 `add`（算一下）一次 `echo`（回一句话），都用 MCP 那边的工具
$task = '你手上有一个 MCP 工具集。请：① 用其中的 add 算 17+25；' +
        '② 用其中的 echo 把计算结果原样回显一次；然后用一句话告诉我最终数字。'
Write-Host "== 任务：$task" -ForegroundColor Cyan
Send 'agent:dada' 't1' @{ task = $task }
$answer = WaitCorr 't1'
if ($answer) { Write-Host ('  它说：' + [string]$answer.payload.text) -ForegroundColor DarkGray }
$proc.StandardInput.Close()
$proc.WaitForExit(30000) | Out-Null
$drain = $proc.StandardOutput.ReadToEndAsync(); $drain.Wait(5000) | Out-Null
$et = $proc.StandardError.ReadToEndAsync(); $et.Wait(5000) | Out-Null
$logPath = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl'
$records = @()
if (Test-Path $logPath) {
    $records = @(Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json })
}
$calls = @($records | Where-Object { $_.type -eq 'request' -and $_.from -eq 'agent:dada' -and $_.payload.tool })
$toBridge = @($records | Where-Object { $_.type -eq 'request' -and $_.to -eq 'mcp:1' -and $_.payload.tool })
# ⚠️ **中控的探针也走同一种 `request`**（`ctrl → mcp:1` 的 `ping`）—— 判"白名单"时
# 必须只看**智能体发给桥的**那些，否则探针会被当成"模型调了个不在表里的工具"
# （第一版就是这么红的）。
$agentToBridge = @($toBridge | Where-Object { $_.from -eq 'agent:dada' })
Write-Host '== 它调了什么 ==' -ForegroundColor Cyan
Write-Host ('  智能体发出：' + (($calls | ForEach-Object { $_.payload.tool }) -join ' / ')) -ForegroundColor DarkGray
Write-Host ('  桥收到的（含中控探针）：' + (($toBridge | ForEach-Object { $_.from + ':' + $_.payload.tool }) -join ' / ')) -ForegroundColor DarkGray
Check '1. 模型给出了结论' {
    $null -ne $answer -and $answer.payload.effect -eq 'confirmed' -and ([string]$answer.payload.text).Length -gt 0
}
Check '2. 它**调了 MCP 那边的工具**（至少两次：add 和 echo）' {
    (@($calls | Where-Object { $_.payload.tool -eq 'add' }).Count -ge 1) -and `
        (@($calls | Where-Object { $_.payload.tool -eq 'echo' }).Count -ge 1)
}
Check '3. **环闭上了**：对端的结果回到了智能体手里（它才能说出 42）' {
    @($records | Where-Object { $_.type -eq 'result' -and $_.to -eq 'agent:dada' -and $_.from -eq 'mcp:1' }).Count -ge 2
}
Check '4. 结论里的数字是**对端算的**（42，不是它自己口算的）' {
    ([string]$answer.payload.text) -like '*42*'
}
Check '5. **白名单在真模型面前也成立**：不在表里的工具一次都没被放行' {
    # 模型可能试着调 `boom`/`sleep`（它看得见名字），但不该有一次真的过去
    @($agentToBridge | Where-Object { $_.payload.tool -notin @('echo', 'add') }).Count -eq 0
}
Check '6. 全程没有 `service_down` / `schema_violation`' {
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
