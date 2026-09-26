# **智能体实例配置**：一份 JSON 说清"我是谁"（2026-09-26）。
#
#   pwsh -File scripts/smoke-agent-config.ps1
#
# 在这之前，"我是谁"全塞在命令行里（`--persona` / `--model` / `--model-window`），
# 而那不是一份**配置** —— 没有 schema、没有校验、改一个字段要去数引号，
# 而且"这个实例当初是怎么配的"除了那串参数没别的地方记着。
#
# 这一份要证五件事：
#
#   1. **配置真的被用了**（人格/窗口/内核按配置走，不是按命令行）；
#   2. **覆盖要出声**（命令行和配置给的不一样时，逐项报出来 —— 沉默覆盖就是两份真相）；
#   3. **不认识的就拒绝启动**，而且**不许声明做不到的事**（`kernel.kind` 写个没人实现的名字、
#      `sees_content: false` 这种假话，都要当场拒，并说清为什么）；
#   4. **它自报、中控记账**：`list` 里那个智能体节点上看得见它的内核/模型/人格；
#   5. **同一份配置能重建同一个实例**：照同一份配置再起一次，系统提示**逐字节一致**；
#      换成另一份配置，提示**跟着变**。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$agentBin = Join-Path $root 'target\debug\harness-agent.exe'
foreach ($b in @($ctrl, $agentBin)) { if (-not (Test-Path $b)) { throw "产物不在：$b" } }
function J([string]$p) { return ($p -replace '\\', '\\') }
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-acfg-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $work -Force | Out-Null
$sid = 's-acfg'
$script1 = Join-Path $work 'script.ndjson'
Set-Content -LiteralPath $script1 -Value '{"step":"text","text":"好。"}' -Encoding utf8
# ---- 三份配置 + 两份坏配置 ----
$cfgA = Join-Path $work 'a.json'
@'
{
  "persona": { "name": "配置甲", "version": "7", "brief": "配置里的人格" },
  "model":   { "kind": "mock" },
  "kernel":  { "kind": "linear", "window": 6000 }
}
'@ | Set-Content -LiteralPath $cfgA -Encoding utf8
$cfgB = Join-Path $work 'b.json'
@'
{
  "persona": { "name": "配置乙", "version": "7", "brief": "另一份配置" },
  "model":   { "kind": "mock" },
  "kernel":  { "kind": "linear", "window": 7000 }
}
'@ | Set-Content -LiteralPath $cfgB -Encoding utf8
$cfgBadKernel = Join-Path $work 'bad-kernel.json'
Set-Content -LiteralPath $cfgBadKernel -Value '{"kernel":{"kind":"bigeye"}}' -Encoding utf8
$cfgLiar = Join-Path $work 'liar.json'
Set-Content -LiteralPath $cfgLiar -Value '{"kernel":{"kind":"linear","sees_content":false}}' -Encoding utf8
$cfgTypo = Join-Path $work 'typo.json'
Set-Content -LiteralPath $cfgTypo -Value '{"personna":{"name":"x"}}' -Encoding utf8

Write-Host '== ③ 坏配置：当场拒，并说清为什么 ==' -ForegroundColor Cyan
# 直接起智能体（不起中控）：配置是在**读参数之后、干任何事之前**校验的，
# 所以这三条走的是"拒绝启动"那条路 —— 它必须**快**而且**说得出话**。
function Start-AgentRaw([string]$cfgPath) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $agentBin
    $psi.Arguments = "--instance agent:cfg --sid s-x --config `"$cfgPath`""
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $p = [System.Diagnostics.Process]::Start($psi)
    $p.StandardInput.Close()
    $done = $p.WaitForExit(10000)
    if (-not $done) { $p.Kill(); return @{ code = -1; err = '（它没退出）' } }
    return @{ code = $p.ExitCode; err = $p.StandardError.ReadToEnd() }
}
$bad1 = Start-AgentRaw $cfgBadKernel
Check '1. `kernel.kind` 写了没人实现的名字（bigeye）⇒ **拒绝启动**，并说清现在只有哪些' {
    $bad1.code -ne 0 -and $bad1.err -like '*bigeye*' -and $bad1.err -like '*linear*'
}
$bad2 = Start-AgentRaw $cfgLiar
Check '2. `sees_content: false` 是**假话** ⇒ 拒绝启动（这一版的内核就在智能体进程里）' {
    $bad2.code -ne 0 -and $bad2.err -like '*sees_content*'
}
$bad3 = Start-AgentRaw $cfgTypo
Check '3. 字段写错（`personna`）⇒ 拒绝启动，不猜你要什么' {
    $bad3.code -ne 0 -and $bad3.err -like '*personna*'
}

Write-Host '== ①②④⑤ 正常起：配置生效 + 覆盖出声 + 自报记账 ==' -ForegroundColor Cyan
$dataDir = Join-Path $work 'data'
$servicesPath = Join-Path $work 'services.json'
function Write-Services([string]$cfgPath) {
    @"
[
  { "kind": "agent", "id": "agent:dada", "role": "agent", "bindings": [],
    "cmd": ["$(J $agentBin)"],
    "args": ["--persona", "命令行甲", "--model", "mock",
             "--script", "$(J $script1)", "--config", "$(J $cfgPath)"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
}
function Start-Ctrl([string[]]$extra) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $ctrl
    $base = @("--sid", $sid, "--data-dir", "`"$dataDir`"", "--services", "`"$servicesPath`"",
              "--drain-ms", "3000", "--tick-ms", "0")
    $psi.Arguments = (($base + $extra) -join ' ')
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
    return [System.Diagnostics.Process]::Start($psi)
}
function Ask([System.Diagnostics.Process]$proc, [string]$corr, [hashtable]$payload) {
    $m = [ordered]@{ id = "m-$corr"; ts = 0; type = 'request'; from = 'console'; to = 'ctrl'
                     corr = $corr; sid = $script:sid; authority = 'user'; payload = $payload }
    $proc.StandardInput.WriteLine(($m | ConvertTo-Json -Compress -Depth 12))
    $proc.StandardInput.Flush()
    $deadline = [DateTime]::UtcNow.AddMilliseconds(20000)
    while ([DateTime]::UtcNow -lt $deadline) {
        $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
        $task = $proc.StandardOutput.ReadLineAsync()
        if (-not $task.Wait($left)) { continue }
        $line = $task.Result
        if ($null -eq $line) { break }
        if ($line.Trim() -eq '') { continue }
        $r = $line | ConvertFrom-Json
        if ($r.type -eq 'result' -and $r.corr -eq $corr) { return $r }
    }
    throw "等 $corr 超时"
}
function LogRecords {
    $logPath = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl'
    return @(Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } |
        ForEach-Object { $_ | ConvertFrom-Json })
}
# **读日志之前先让中控落盘。** 日志是它独占的（只有它写、只有它 fsync），
# 而它手里还压着写缓冲 —— 直接读文件会读到**旧的**（第一次跑就是栽在这儿：
# 系统提示那条记录还没落盘，第 5 条判据就去找它了）。
# 这不是脚本的技巧，是**协议里那条正经的路**：`{"op":"barrier"}`。
function Barrier([System.Diagnostics.Process]$proc, [string]$corr) {
    $null = Ask $proc $corr @{ op = 'barrier'; reason = '读日志之前' }
}
function SystemsSoFar {
    return @(LogRecords | Where-Object { $_.payload.kind -eq 'prompt_compiled' } |
        ForEach-Object { $_.payload.system })
}
# ---- 第一轮：配置甲 ----
Write-Services $cfgA
$p1 = Start-Ctrl @()
Start-Sleep -Seconds 2
$g1 = Ask $p1 'g-1' @{ op = 'list' }
$node = @($g1.payload.data.instances | Where-Object { $_.id -eq 'agent:dada' })
Check '4. **它自报、中控记账**：`list` 上那个智能体带着它的内核 / 模型 / 人格' {
    $node.Count -eq 1 -and $node[0].config.kernel.kind -eq 'linear' -and
    $node[0].config.kernel.window -eq 6000 -and $node[0].config.persona.name -eq '配置甲' -and
    $node[0].config.model.kind -eq 'mock'
}
Barrier $p1 'b-1'
# ⚠️ **`@(...)` 不能省**：PowerShell 的函数返回值会把**单元素数组摊开成标量** ——
# 这一轮只有一条系统提示，不加 `@()` 的话 `$sysA` 就是那个**字符串**，
# 而 `$sysA[0]` 取到的是它的**第一个字符**（判定当场假红，我第一次就是这么栽的）。
$sysA = @(SystemsSoFar)
Check '5. **配置真的被用了**：编译出来的系统提示里是配置里那个人格（不是命令行那个）' {
    $sysA.Count -ge 1 -and $sysA[0] -like '*配置甲*' -and $sysA[0] -notlike '*命令行甲*'
}
$p1.StandardInput.Close()
$p1.WaitForExit(8000) | Out-Null
$err1 = $p1.StandardError.ReadToEnd()
Check '6. **覆盖要出声**：命令行给过 `--persona`，被配置盖掉时逐项报出来' {
    $err1 -like '*实例配置覆盖了命令行*' -and $err1 -like '*--persona*'
}

# ---- 第二轮：**同一份配置**再来一次（这是"保存下来的配置能重建实例"）----
Write-Services $cfgA
$p2 = Start-Ctrl @('--resume')
Start-Sleep -Seconds 2
$p2.StandardInput.Close()
$p2.WaitForExit(8000) | Out-Null
$sysAfter = @(SystemsSoFar)
Check '7. **同一份配置 ⇒ 同一个实例**：两辈子的系统提示**逐字节一致**' {
    $sysAfter.Count -ge 2 -and $sysAfter[0] -ceq $sysAfter[1]
}

# ---- 第三轮：换一份配置（**行为跟着配置变**）----
Write-Services $cfgB
$p3 = Start-Ctrl @()
Start-Sleep -Seconds 2
Barrier $p3 'b-3'
$sysB = @(SystemsSoFar)
$last = $sysB[$sysB.Count - 1]
Check '8. 换一份配置 ⇒ **提示跟着变**（这条是"配置是活的、不是摆设"的凭证）' {
    $last -like '*配置乙*' -and $last -notlike '*配置甲*' -and $last -cne $sysAfter[0]
}
$p3.StandardInput.Close()
$p3.WaitForExit(8000) | Out-Null
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    Write-Host '== 第一轮 stderr ==' -ForegroundColor DarkGray
    $err1 -split "`n" | ForEach-Object { if ($_.Trim()) { Write-Host ('  ' + $_) -ForegroundColor DarkGray } }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host ("工作目录留在：" + $work) -ForegroundColor DarkGray
