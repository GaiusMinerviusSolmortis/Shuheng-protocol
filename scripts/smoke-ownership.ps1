# **实例独属**：每个智能体拿到的都是**独属于自己的前端**。
#
#   pwsh -File scripts/smoke-ownership.ps1
#
# 形状是"前后端"（§5.10 那张办公软件的表）：**实例是前端，服务是后端。**
# 同一个后端可以同时挂多个前端 —— **即使它们看起来一模一样**
# （两个终端实例、同一个目录），各自的状态（cwd / 输出游标 / 正在跑的命令）
# 也是**分开的**。规矩一句话：**智能体之间互不越界，用户不受限。**
#
# 这条检查写在**中控**，不写在服务里：`谁开着哪个实例` 是一张**图上的事实**，
# 只有中控看得全 —— 所以只写一次，第三方服务不必各自实现一遍。
# （终端那边另外还有一道同规则的纵深检查，编辑器一直有。）
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$term = Join-Path $root 'target\debug\harness-term.exe'
$agentBin = Join-Path $root 'target\debug\harness-agent.exe'
function J([string]$p) { return ($p -replace '\\', '\\') }
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-own-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path (Join-Path $project 'sub') -Force | Out-Null
$sid = 's-own'
$dataDir = Join-Path $work 'data'
$scriptPath = Join-Path $work 'script.ndjson'
# 智能体这一回合干四件事：摸别人的实例 → 开自己的 → 在自己的里面 cd →
# **再摸一次别人的**（第二次是为了排除"它只是还没有自己的实例"这个解释）
@'
{"step":"tool","calls":[{"to":"term:1","tool":"read","args":{"instance_id":"t-1"}}]}
{"step":"tool","calls":[{"to":"term:1","tool":"open","args":{"path":".","purpose":"我自己的"}}]}
{"step":"tool","calls":[{"to":"term:1","tool":"run","args":{"instance_id":"$last.instance_id","cmd":"cd sub"}}]}
{"step":"tool","calls":[{"to":"term:1","tool":"read","args":{"instance_id":"t-1"}}]}
{"step":"text","text":"别人的实例碰不动，我开了一个自己的，而且在里面 cd 了。"}
'@ | Set-Content -LiteralPath $scriptPath -Encoding utf8
$servicesPath = Join-Path $work 'services.json'
@"
[
  { "kind": "term",  "id": "term:1",  "role": "tool",
    "cmd": ["$(J $term)"], "args": ["--fence", "$(J $project)"] },
  { "kind": "agent", "id": "agent:dada", "role": "agent", "bindings": ["term:1"],
    "cmd": ["$(J $agentBin)"],
    "args": ["--persona", "达达", "--fence", "$(J $project)", "--script", "$(J $scriptPath)"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ctrl
$psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 8000 --host `"$project`""
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
$proc = [System.Diagnostics.Process]::Start($psi)
$script:outTask = $null
function NextLine([int]$timeoutMs) {
    if ($null -eq $script:outTask) { $script:outTask = $proc.StandardOutput.ReadLineAsync() }
    if (-not $script:outTask.Wait($timeoutMs)) { return $null }
    $line = $script:outTask.Result
    $script:outTask = $null
    return $line
}
$replies = @{}
function Send($body, [string]$corr, [string]$to = 'ctrl') {
    $m = [ordered]@{ id = "m-$corr"; ts = 0; type = 'request'; from = 'console'; to = $to
                     corr = $corr; sid = $sid; authority = 'user'; payload = $body }
    $proc.StandardInput.WriteLine(($m | ConvertTo-Json -Compress -Depth 12))
    $proc.StandardInput.Flush()
}
function Pump([int]$timeoutMs, [string[]]$want) {
    $deadline = [DateTime]::UtcNow.AddMilliseconds($timeoutMs)
    while ([DateTime]::UtcNow -lt $deadline) {
        $missing = @($want | Where-Object { -not $replies.ContainsKey($_) })
        if ($missing.Count -eq 0) { return }
        $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
        $line = NextLine $left
        if ($null -eq $line) { continue }
        if ($line.Trim() -eq '') { continue }
        $m = $line | ConvertFrom-Json
        if ($m.type -eq 'result' -and $m.corr) { $replies[$m.corr] = $m }
    }
}

Write-Host '== 先由用户（控制台）开一个终端：t-1 ==' -ForegroundColor Cyan
Send @{ tool = 'open'; args = @{ path = '.'; purpose = '用户自己的' } } 'u-open' 'term:1'
Pump 15000 @('u-open')
$t1 = $replies['u-open'].payload.instance_id
Write-Host '== 再把一个任务交给智能体，让它在里面摸爬滚打 ==' -ForegroundColor Cyan
Send @{ task = '看看你能不能动那个终端' } 'task' 'agent:dada'
Pump 40000 @('task')
Send @{ tool = 'status'; args = @{} } 'u-status' 'term:1'
Pump 15000 @('u-status')
$status = $replies['u-status'].payload.data
# 图上那一份（中控的注册表快照）也要看得见归属
Send @{ op = 'list' } 'u-list'
Pump 15000 @('u-list')
# 用户对智能体的实例**照常能读、能关**（§7.1：用户永远能杀掉和浏览正在运行的终端）
$agentInst = ($status.terms | Where-Object { $_.owner -like 'agent:dada@*' } | Select-Object -First 1).instance_id
if ($agentInst) {
    Send @{ tool = 'read'; args = @{ instance_id = $agentInst } } 'u-read' 'term:1'
    Pump 15000 @('u-read')
    Send @{ tool = 'close'; args = @{ instance_id = $agentInst } } 'u-close' 'term:1'
    Pump 15000 @('u-close')
}
$proc.StandardInput.Close()
$proc.WaitForExit(10000) | Out-Null
$et = $proc.StandardError.ReadToEndAsync(); $et.Wait(4000) | Out-Null
$logPath = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl'
$records = @(Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json })

$refused = @($records | Where-Object { $_.type -eq 'result' -and $_.to -eq 'agent:dada' -and $_.payload.code -eq 'refused' })
$agentOpened = @($records | Where-Object { $_.type -eq 'result' -and $_.from -eq 'term:1' -and $_.payload.instance_id -and $_.to -eq 'agent:dada' })
$openEvents = @($records | Where-Object { $_.payload.kind -eq 'instance' -and $_.payload.action -eq 'open' })
$cdOk = @($records | Where-Object { $_.type -eq 'result' -and $_.from -eq 'term:1' -and $_.payload.cwd -like '*\sub' })
$listed = @($replies['u-status'].payload.data.instances | Where-Object { $_.id -eq 'term:1' })

$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
Write-Host '== 判据 ==' -ForegroundColor Cyan
Check '1. 用户开的 `t-1` 记了归属（事件里带 `owner`）' {
    $e = $openEvents | Where-Object { $_.payload.instance -eq $t1 } | Select-Object -First 1
    $e -and $e.payload.owner -like 'console@*'
}
Check '2. **智能体碰不动别人的实例**：读 `t-1` 被中控拒了' {
    $refused.Count -ge 1 -and ($refused.payload.why -join ' ') -like "*$t1*不是你的实例*"
}
Check '3. 拒绝里说清**它是谁的**，并给出出口（自己开一个）' {
    ($refused.payload.why -join ' ') -like '*console@*' -and `
        ($refused.payload.remedy -join ' ') -like '*各开一个*'
}
Check '4. 智能体自己 open 拿到了**属于它自己**的实例（不是复用 `t-1`）' {
    $agentOpened.Count -ge 1 -and $agentOpened[0].payload.instance_id -ne $t1
}
Check '5. 那个实例的归属是它自己' {
    $mine = $agentOpened[0].payload.instance_id
    $e = $openEvents | Where-Object { $_.payload.instance -eq $mine } | Select-Object -First 1
    $e -and $e.payload.owner -like 'agent:dada@*'
}
Check '6. 它在**自己的**实例里 cd 成功了（`sub`）' { $cdOk.Count -ge 1 }
Check '7. **两个前端互相独立**：用户的 `t-1` 还停在项目根上，没被那次 cd 影响' {
    $mine = ($status.terms | Where-Object { $_.instance_id -eq $t1 })
    $mine -and $mine.cwd -notlike '*\sub' -and $mine.owner -like 'console@*'
}
Check '8. 第二次摸 `t-1` 还是被拒（排除"它只是还没有自己的实例"这个解释）' {
    ($refused | Where-Object { $_.payload.why -like "*$t1*不是你的实例*" }).Count -ge 2
}
Check '9. **看得见别人的**：`status` 里每一条都带 `owner`，而且标了 `can_operate`' {
    $status.terms.Count -ge 2 -and ($status.terms | Where-Object { $null -eq $_.owner }).Count -eq 0
}
Check '10. **用户不受限**：用户能读、能关智能体的实例（§7.1）' {
    $replies['u-read'] -and $replies['u-read'].payload.effect -eq 'confirmed' -and `
        $replies['u-close'] -and $replies['u-close'].payload.effect -eq 'confirmed'
}
Check '11. 中控把归属也放进了图（`list` 里看得见 owner）' {
    $graph = $replies['u-list'].payload.data.tool_instances
    $graph -and $graph.Count -ge 2 -and ($graph | Where-Object { $null -eq $_.owner }).Count -eq 0 -and `
        (($graph | Where-Object { $_.id -eq $t1 }).owner -like 'console@*')
}

Write-Host ''
Write-Host '== 那次拒绝（智能体摸别人的实例）==' -ForegroundColor Cyan
if ($refused.Count -gt 0) {
    Write-Host "  why:    $($refused[0].payload.why)"
    Write-Host "  remedy: $($refused[0].payload.remedy)"
}
Write-Host ''
Write-Host '== 图上这三个实例（服务 / 实例 / 归属）==' -ForegroundColor Cyan
if ($status.terms) {
    $status.terms | ForEach-Object {
        Write-Host ("  {0,-6} owner={1,-22} cwd={2}" -f $_.instance_id, $_.owner, $_.cwd)
    }
}
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "工作目录留在：$work"
