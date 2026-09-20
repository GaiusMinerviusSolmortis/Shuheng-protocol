# **重新托管**：项目搬走了，会话里委托一条新路径 —— 权限和位置一起跟着换。
#
#   pwsh -File scripts/smoke-rehost.ps1
#
# 这条验收的是那句"**记忆跟着会话走，项目搬走就在会话里委托新的路径**"。
# 它以前执行不了任何东西：`host` 只是记账，服务的围栏是服务表里写死的参数 ——
# 中控**根本不知道**那串 `--fence ...` 是什么。
#
# 现在三件事连上了：
#   * 服务在 manifest 里报**规范化之后的围栏**；
#   * 围栏由会话给的那些服务（`wants_fence`），**装配时从会话取路径**；
#   * `host` 立即生效 —— 中控把那些服务收掉、照新路径重新装配。
#
# 顺带验第四件：**光有实例名白名单还不够** —— 一个绑在别的路径上的工具实例，
# 本会话的智能体也够不着。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$term = Join-Path $root 'target\debug\harness-term.exe'
$agentBin = Join-Path $root 'target\debug\harness-agent.exe'
function J([string]$p) { return ($p -replace '\\', '\\') }
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-rehost-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$projA = Join-Path $work 'projA'
$projB = Join-Path $work 'projB'
$projC = Join-Path $work 'projC'
foreach ($d in @($projA, $projB, $projC)) { New-Item -ItemType Directory -Path $d -Force | Out-Null }
$sid = 's-rehost'
$dataDir = Join-Path $work 'data'
$scriptPath = Join-Path $work 'script.ndjson'
@'
{"step":"tool","calls":[{"to":"term:1","tool":"status","args":{}}]}
{"step":"tool","calls":[{"to":"term:2","tool":"status","args":{}}]}
{"step":"text","text":"试过了。"}
'@ | Set-Content -LiteralPath $scriptPath -Encoding utf8
$servicesPath = Join-Path $work 'services.json'
# term:1 —— 围栏**由会话给**；term:2 —— 围栏写死指向 projC（没人托管它）
@"
[
  { "kind": "term", "id": "term:1", "role": "tool", "wants_fence": true,
    "cmd": ["$(J $term)"] },
  { "kind": "term", "id": "term:2", "role": "tool",
    "cmd": ["$(J $term)"], "args": ["--fence", "$(J $projC)"] },
  { "kind": "agent", "id": "agent:dada", "role": "agent", "bindings": ["term:1", "term:2"],
    "cmd": ["$(J $agentBin)"],
    "args": ["--persona", "达达", "--script", "$(J $scriptPath)"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ctrl
# **开会话的时候就带上托管路径**（projA）
$psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 8000 --host `"$projA`""
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
$proc = [System.Diagnostics.Process]::Start($psi)
$script:outTask = $null
$script:seq = 0
$script:replies = @{}
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
        id = "m-$($script:seq)"; ts = 0; type = 'request'; from = 'console'
        to = $to; corr = $corr; sid = $sid; authority = 'user'; payload = $payload
    }
    $proc.StandardInput.WriteLine(($msg | ConvertTo-Json -Compress -Depth 12))
    $proc.StandardInput.Flush()
}
function Wait([string]$corr, [int]$timeoutMs = 30000) {
    if ($script:replies.ContainsKey($corr)) { return $script:replies[$corr] }
    $deadline = [DateTime]::UtcNow.AddMilliseconds($timeoutMs)
    while ([DateTime]::UtcNow -lt $deadline) {
        $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
        $line = NextLine $left
        if ($null -eq $line) { continue }
        if ($line.Trim() -eq '') { continue }
        $m = $line | ConvertFrom-Json
        if ($m.type -eq 'result') { $script:replies[$m.corr] = $m }
        if ($m.type -eq 'result' -and $m.corr -eq $corr) { return $m }
    }
    throw "等 $corr 超时"
}
function Ask([string]$to, [string]$corr, [hashtable]$payload) { Send $to $corr $payload; return Wait $corr }
function Graph([string]$corr) { return (Ask 'ctrl' $corr @{ op = 'list' }).payload.data }
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
Write-Host '== 判据 ==' -ForegroundColor Cyan
Start-Sleep -Milliseconds 900
$g0 = Graph 'g-0'
Check '1. 开会话时给的托管路径，成了那个服务的围栏（不是服务表里写的）' {
    $g0.instances | Where-Object { $_.id -eq 'term:1' -and $_.owns.fences -like '*projA*' } | Measure-Object | ForEach-Object { $_.Count -ge 1 }
}
Check '2. 另一个服务的围栏**写死在服务表里**，指向没被托管的 projC' {
    $g0.instances | Where-Object { $_.id -eq 'term:2' -and $_.owns.fences -like '*projC*' } | Measure-Object | ForEach-Object { $_.Count -ge 1 }
}
# 让智能体去调两个服务：term:1 在托管范围内，term:2 不在
$r = Ask 'agent:dada' 'c-task' @{ task = '两个终端都试一下' }
$logPath = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl'
$scopeRefuse = $null
Write-Host ''
Write-Host '== 重新托管：换一条路径 ==' -ForegroundColor Cyan
# 搬家是两步：**先 unhost 旧的，再 host 新的**。
# 顺序有讲究：服务的围栏根**必须存在**，所以不能先把旧路径留在那儿 ——
# 真搬家之后旧目录早就不在了。
$u = Ask 'ctrl' 'c-unhost' @{ op = 'unhost'; path = $projA }
$h = Ask 'ctrl' 'c-host' @{ op = 'host'; path = $projB }
Start-Sleep -Milliseconds 1200
$g1 = Graph 'g-1'
Check '3. **host 立即生效**：那个要围栏的服务被重新装配了' {
    $u.payload.data.services_rehosted -ge 1 -or $h.payload.data.services_rehosted -ge 1
}
Check '4. 它的围栏**换成了新路径**（服务自己报的 manifest 里）' {
    $g1.instances | Where-Object { $_.id -eq 'term:1' -and $_.owns.fences -like '*projB*' } | Measure-Object | ForEach-Object { $_.Count -ge 1 }
}
Check '5. 旧的工具实例跟着收掉了（它本来就绑在旧路径上）' {
    $g1.tool_instances | Where-Object { $_.owner -eq 'term:1' } | Measure-Object | ForEach-Object { $_.Count -eq 0 }
}
$proc.StandardInput.Close()
$proc.WaitForExit(10000) | Out-Null
$et = $proc.StandardError.ReadToEndAsync(); $et.Wait(4000) | Out-Null
$records = @(Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json })
Check '6. 换路径留了痕（一条 rehost 事件，不是靠算法去猜）' {
    ($records | Where-Object { $_.payload.kind -eq 'rehost' } | Measure-Object).Count -ge 1
}
Check '7. **光有实例名白名单还不够**：绑在别的路径上的服务，够不着' {
    $ref = $records | Where-Object { $_.to -eq 'agent:dada' -and $_.payload.code -eq 'refused' -and $_.payload.why -like '*不在本会话托管的范围*' }
    $ref -ne $null -and $ref.payload.remedy -like '*host*'
}
Check '8. 在托管范围内的那个照常能用' {
    ($records | Where-Object { $_.type -eq 'result' -and $_.from -eq 'term:1' -and $_.payload.effect -eq 'confirmed' } | Measure-Object).Count -ge 1
}
Write-Host ''
Write-Host '== 中控的 stderr ==' -ForegroundColor Cyan
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
