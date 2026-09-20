# **身份索引**：跨会话、只追加的一张账 —— `id → 它是什么 / 归谁 / 什么时候出现`。
#
#   pwsh -File scripts/smoke-index.ps1
#
# 为什么会话日志不够：日志是**按会话**切的，而且记的是**消息**。
# 要回答"`term:1/t-2` 是谁、哪个会话开的它"，得把每个会话翻一遍。
# 所以再来一份**按身份**的账（`data/index/instances.jsonl`，一行一个事件）。
# 它不是日志的替代品 —— 日志仍然是唯一真相，索引是**派生出来的读模型**。
#
# 这份脚本要证三件事：
#   1. 四层（会话 / 智能体 / 工具服务 / 工具实例）都在账上，而且带得出**那条链**
#   2. 同一个 id 会留下**多条**记录（只追加、不覆盖）
#   3. **换个会话也查得到** —— 这才是"跨会话追溯"的全部意义
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$term = Join-Path $root 'target\debug\harness-term.exe'
$agentBin = Join-Path $root 'target\debug\harness-agent.exe'
function J([string]$p) { return ($p -replace '\\', '\\') }
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-index-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null
$dataDir = Join-Path $work 'data'
$scriptPath = Join-Path $work 'script.ndjson'
@'
{"step":"tool","calls":[{"to":"term:1","tool":"open","args":{"path":".","purpose":"索引验收"}}]}
{"step":"text","text":"开好了。"}
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

function Start-Ctrl([string]$sid, [string]$services) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $ctrl
    $psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$services`" --drain-ms 6000"
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
    return [System.Diagnostics.Process]::Start($psi)
}
function Pump([System.Diagnostics.Process]$p, [hashtable]$sink, [string[]]$want, [int]$timeoutSec = 40) {
    $t = $null
    $deadline = [DateTime]::UtcNow.AddSeconds($timeoutSec)
    while ([DateTime]::UtcNow -lt $deadline) {
        if (@($want | Where-Object { -not $sink.ContainsKey($_) }).Count -eq 0) { return }
        if ($t -eq $null) { $t = $p.StandardOutput.ReadLineAsync() }
        if (-not $t.Wait(500)) { continue }
        $line = $t.Result
        $t = $null
        if ($null -eq $line -or $line.Trim() -eq '') { continue }
        $m = $line | ConvertFrom-Json
        if ($m.type -eq 'result' -and $m.corr) { $sink[$m.corr] = $m }
        if ($m.type -eq 'presence' -and $m.payload.manifest) { $sink['presence'] = $m }
    }
}
function Send([System.Diagnostics.Process]$p, [string]$sid, $body, [string]$corr, [string]$to = 'ctrl') {
    $m = [ordered]@{ id = "m-$corr"; ts = 0; type = 'request'; from = 'console'; to = $to
                     corr = $corr; sid = $sid; authority = 'user'; payload = $body }
    $p.StandardInput.WriteLine(($m | ConvertTo-Json -Compress -Depth 12))
    $p.StandardInput.Flush()
}

Write-Host '== 会话一：用户开一个、智能体开一个、再改一次授权 ==' -ForegroundColor Cyan
$sid1 = 's-index-1'
$p1 = Start-Ctrl $sid1 $servicesPath
$seen1 = @{}
Pump $p1 $seen1 @('presence') 25
Send $p1 $sid1 @{ tool = 'open'; args = @{ path = '.'; purpose = '用户的' } } 'u-open' 'term:1'
Pump $p1 $seen1 @('u-open') 25
Send $p1 $sid1 @{ task = '开一个你自己的终端' } 'task' 'agent:dada'
Pump $p1 $seen1 @('task') 60
Send $p1 $sid1 @{ op = 'grant'; agent = 'agent:dada'; instance = 'term:1'; capabilities = @('proc.exec') } 'g1'
Pump $p1 $seen1 @('g1') 25
# 会话内查一次实例：要能拿到那条链
Send $p1 $sid1 @{ op = 'index'; id = 'term:1/t-2' } 'q1'
Pump $p1 $seen1 @('q1') 25
$p1.StandardInput.Close()
$p1.WaitForExit(10000) | Out-Null
$et1 = $p1.StandardError.ReadToEndAsync(); $et1.Wait(4000) | Out-Null

Write-Host '== 会话二：**换个会话**，还查得到上一条的记录 ==' -ForegroundColor Cyan
$sid2 = 's-index-2'
$p2 = Start-Ctrl $sid2 $servicesPath
$seen2 = @{}
Pump $p2 $seen2 @('presence') 25
Send $p2 $sid2 @{ op = 'index'; sid = $sid1; limit = 200 } 'q2'
Pump $p2 $seen2 @('q2') 25
$p2.StandardInput.Close()
$p2.WaitForExit(10000) | Out-Null
$et2 = $p2.StandardError.ReadToEndAsync(); $et2.Wait(4000) | Out-Null

$indexPath = Join-Path (Join-Path $dataDir 'index') 'instances.jsonl'
$raw = @(Get-Content -LiteralPath $indexPath | Where-Object { $_.Trim() -ne '' })
$recs = @($raw | ForEach-Object { $_ | ConvertFrom-Json })
$q1 = $seen1['q1'].payload.data
$q2 = $seen2['q2'].payload.data

$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
Write-Host '== 判据 ==' -ForegroundColor Cyan
Check '1. 索引是一份**只追加的 JSONL**，每行都能解析' {
    $raw.Count -ge 8 -and ($raw | Where-Object { $_.Trim() -notlike '{*' }).Count -eq 0
}
Check '2. **四层都在账上**：通道 / 智能体 / 工具服务 / 工具实例' {
    $ids = @($recs | ForEach-Object { $_.id })
    ($ids -contains 'console') -and ($ids -contains 'agent:dada') -and `
        ($ids -contains 'term:1') -and ($ids -contains 'term:1/t-1')
}
Check '3. 登记那条带 `kind` / `role` / `cmd`；报到那条带 manifest 身份' {
    $reg = $recs | Where-Object { $_.id -eq 'term:1' -and $_.event -eq 'registered' } | Select-Object -First 1
    $pres = $recs | Where-Object { $_.id -eq 'term:1' -and $_.event -eq 'presence' } | Select-Object -First 1
    $reg -and $reg.role -eq 'tool' -and $reg.cmd -like '*harness-term*' -and `
        $pres -and $pres.manifest_id -eq 'term'
}
Check '4. 实例记录带 **`owner`（归谁）和 `service`（挂在谁身上）**' {
    $i = $recs | Where-Object { $_.id -eq 'term:1/t-2' -and $_.event -eq 'opened' } | Select-Object -First 1
    $i -and $i.owner -like 'agent:dada@*' -and $i.service -eq 'term:1' -and $i.purpose -eq '索引验收'
}
Check '5. 查一个实例，账上能算出**那条链**（会话 → 智能体 → 服务 → 实例）' {
    $c = $q1.chain
    $c -and $c.session -eq $sid1 -and $c.agent -eq 'agent:dada' -and `
        $c.service -eq 'term:1' -and $c.instance -eq 't-2'
}
Check '6. 同一个 id 有多条记录（**只追加、不覆盖**）' {
    ($recs | Where-Object { $_.id -eq 'term:1' }).Count -ge 3 -and `
        ($recs | Where-Object { $_.event -eq 'registered' }).Count -ge 1 -and `
        ($recs | Where-Object { $_.event -eq 'exited' }).Count -ge 1
}
Check '7. **换个会话也查得到**（这才是跨会话追溯）' {
    $q2.matched -ge 5 -and (($q2.records | ForEach-Object { $_.sid }) -contains $sid1)
}
Check '8. **授权变更也留痕**（`action` + 改完之后的整张清单）' {
    $b = $recs | Where-Object { $_.id -eq 'agent:dada' -and $_.event -eq 'binding' } | Select-Object -First 1
    $b -and $b.action -eq 'grant' -and $b.instance -eq 'term:1' -and ($b.bindings -contains 'term:1')
}

Write-Host ''
Write-Host '== 索引里的那几行（原样）==' -ForegroundColor Cyan
$raw | Select-Object -First 12 | ForEach-Object { Write-Host "  $_" }
Write-Host ''
Write-Host '== 从索引里算出来的那条链 ==' -ForegroundColor Cyan
if ($q1.chain) {
    Write-Host "  会话      $($q1.chain.session)"
    Write-Host "  智能体    $($q1.chain.agent)"
    Write-Host "  工具服务  $($q1.chain.service)"
    Write-Host "  工具实例  $($q1.chain.instance)"
}
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "索引文件留在：$indexPath"
