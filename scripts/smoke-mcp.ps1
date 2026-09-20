# **MCP 桥**的验收（附录 §10）。
#
#   pwsh -File scripts/smoke-mcp.ps1
#
# 要证的是**框架能当 MCP 的宿主**（§10.3 那句"包含，不是区别"），而且
# **不新增任何一种东西**：桥对上层就是一个普通工具服务。
#
# 对端是 `mock-mcp`（本仓库里的最小 MCP server）——
# 它替我们把"别人写的工具"这个角色演出来，而且每个工具都对着一条要证的判据：
#
#   echo  → 正常路径                    add  → 内容里混一块**不是文本**的东西
#   boom  → **工具自己说失败**（isError） crash → **对端进程直接死**
#
# 三条最容易做错的，在这儿钉死：
#   · **工具失败 ≠ 调用失败**（isError 是数据，effect 照样 confirmed）；
#   · **认不出来的内容不许假装看见了**（要留一行说清）；
#   · **对端没了要立刻说得清**（不能等超时，也不能自己编一条答复）。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$bridge = Join-Path $root 'target\debug\harness-mcp.exe'
$mock = Join-Path $root 'target\debug\mock-mcp.exe'
$agentBin = Join-Path $root 'target\debug\harness-agent.exe'
foreach ($b in @($ctrl, $bridge, $mock, $agentBin)) { if (-not (Test-Path $b)) { throw "产物不在：$b" } }
function J([string]$p) { return ($p -replace '\\', '\\') }
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-mcp-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null
$sid = 's-mcp'
$dataDir = Join-Path $work 'data'
# 两个智能体，**差别只在凭证**：一个签了 `mcp.call`，一个一条都没有
$scriptY = Join-Path $work 'script-yes.ndjson'
@'
{"step":"tool","calls":[{"to":"mcp:1","tool":"echo","args":{"text":"从 MCP 回来的话"}}]}
{"step":"text","text":"我调过 MCP 了。"}
'@ | Set-Content -LiteralPath $scriptY -Encoding utf8
$scriptN = Join-Path $work 'script-no.ndjson'
@'
{"step":"tool","calls":[{"to":"mcp:1","tool":"echo","args":{"text":"我也试试"}}]}
{"step":"text","text":"我调不动它。"}
'@ | Set-Content -LiteralPath $scriptN -Encoding utf8
$servicesPath = Join-Path $work 'services.json'
# ⚠️ **故意不放 `sleep`**：它就是白名单那条判据的靶子
@"
[
  { "kind": "mcp", "id": "mcp:1", "role": "tool",
    "cmd": ["$(J $bridge)"],
    "args": ["--server", "$(J $mock)",
             "--allow", "echo", "--allow", "add", "--allow", "boom", "--allow", "crash",
             "--timeout-ms", "5000"] },
  { "kind": "agent", "id": "agent:yes", "role": "agent",
    "bindings": [{"instance": "mcp:1", "capabilities": ["mcp.call"]}],
    "cmd": ["$(J $agentBin)"],
    "args": ["--persona", "有凭证", "--fence", "$(J $project)", "--script", "$(J $scriptY)"] },
  { "kind": "agent", "id": "agent:no", "role": "agent",
    "bindings": [{"instance": "mcp:1", "capabilities": []}],
    "cmd": ["$(J $agentBin)"],
    "args": ["--persona", "没凭证", "--fence", "$(J $project)", "--script", "$(J $scriptN)"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ctrl
$psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 5000 --tick-ms 0"
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
$proc = [System.Diagnostics.Process]::Start($psi)
$script:outTask = $null
$script:seq = 0
function NextLine([int]$timeoutMs) {
    if ($null -eq $script:outTask) { $script:outTask = $proc.StandardOutput.ReadLineAsync() }
    if (-not $script:outTask.Wait($timeoutMs)) { return $null }
    $line = $script:outTask.Result
    $script:outTask = $null
    return $line
}
function Send([string]$to, [string]$corr, [hashtable]$payload) {
    $script:seq++
    $m = [ordered]@{ id = "m-$($script:seq)"; ts = 0; type = 'request'; from = 'console'
                     to = $to; corr = $corr; sid = $sid; authority = 'user'; payload = $payload }
    $proc.StandardInput.WriteLine(($m | ConvertTo-Json -Compress -Depth 12))
    $proc.StandardInput.Flush()
}
function WaitCorr([string]$corr, [int]$timeoutMs = 30000) {
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
for ($i = 0; $i -lt 30; $i++) {
    Start-Sleep -Milliseconds 150
    Send 'ctrl' "ready-$i" @{ op = 'list' }
    $r = WaitCorr "ready-$i" 900
    if ($r -and (@($r.payload.data.instances | ForEach-Object { $_.id }) -contains 'mcp:1')) { break }
}

Write-Host '== 判据 ==' -ForegroundColor Cyan
Send 'ctrl' 'l1' @{ op = 'list' }
$l1 = WaitCorr 'l1'
Send 'mcp:1' 'm1' @{ tool = 'help'; args = @{} }
$m1 = WaitCorr 'm1'
Send 'mcp:1' 'wh' @{ tool = 'sleep'; args = @{ ms = 10 } }
$wh = WaitCorr 'wh'
Send 'mcp:1' 'e1' @{ tool = 'echo'; args = @{ text = '你好，MCP' } }
$e1 = WaitCorr 'e1'
Send 'mcp:1' 'b1' @{ tool = 'boom'; args = @{} }
$b1 = WaitCorr 'b1'
Send 'mcp:1' 'a1' @{ tool = 'add'; args = @{ a = 1; b = 2 } }
$a1 = WaitCorr 'a1'
Send 'mcp:1' 'u1' @{ tool = '没有这个工具'; args = @{} }
$u1 = WaitCorr 'u1'
# 两个只差凭证的智能体，各跑一遍
Send 'agent:yes' 'ty' @{ task = '调一次 MCP 的 echo' }
$ty = WaitCorr 'ty'
Send 'agent:no' 'tn' @{ task = '也调一次' }
$tn = WaitCorr 'tn'
# **最后**才演"对端死了"—— 它会把这个服务从图上带走
Send 'mcp:1' 'c1' @{ tool = 'crash'; args = @{} }
$c1 = WaitCorr 'c1' 15000
Start-Sleep -Milliseconds 800
Send 'ctrl' 'l2' @{ op = 'list' }
$l2 = WaitCorr 'l2'

$proc.StandardInput.Close()
$proc.WaitForExit(8000) | Out-Null
try { $d = $proc.StandardOutput.ReadToEndAsync(); $d.Wait(2000) | Out-Null } catch { }
$et = $proc.StandardError.ReadToEndAsync(); $et.Wait(4000) | Out-Null
$errText = if ($et.IsCompleted) { $et.Result } else { '' }
$logPath = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl'
$records = Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json }

$node = $l1.payload.data.instances | Where-Object { $_.id -eq 'mcp:1' } | Select-Object -First 1
$manifest = ($records | Where-Object { $_.type -eq 'presence' -and $_.from -eq 'mcp:1' -and $_.payload.manifest } |
    Select-Object -First 1).payload.manifest

Check '1. 桥自报的 manifest 过了注册关，而且**工具表是从对端 `tools/list` 生成的**' {
    $node -and $manifest -and $errText -notlike '*manifest 不合规*' -and `
        (@($manifest.tools | ForEach-Object { $_.name }) -contains 'echo') -and `
        (@($manifest.tools | ForEach-Object { $_.name }) -contains 'add') -and `
        (@($manifest.tools | ForEach-Object { $_.name }) -contains 'boom') -and `
        (@($manifest.mcp.tools | ForEach-Object { $_ }) -contains 'crash')
}
Check '2. **白名单默认拒绝**：不在名单里的工具被桥自己挡下，并说清怎么放行' {
    $wh.payload.effect -eq 'refused' -and $wh.payload.why -like '*不在白名单*' -and `
        $wh.payload.remedy -like '*--allow*'
}
Check '3. 放行之后能调通：MCP 的 `content` 文本进了 `output`' {
    $e1.payload.effect -eq 'confirmed' -and $e1.payload.code -eq $null -and `
        $e1.payload.output -eq '你好，MCP'
}
Check '4. **工具失败 ≠ 调用失败**：`isError` 是数据，`effect` 照样 confirmed' {
    $b1.payload.effect -eq 'confirmed' -and $b1.payload.data.is_error -eq $true -and `
        $b1.payload.output -like '*它炸了*'
}
Check '5. **认不出来的内容不许假装看见**：非文本块要留一行说清，原文留在 data.mcp' {
    $a1.payload.output -like '*3*' -and $a1.payload.output -like '*image*' -and `
        $a1.payload.data.mcp.content.Count -eq 2
}
Check '6. 名字写错：拒，并**列出对端真有哪些工具**（不许让模型猜）' {
    $u1.payload.effect -eq 'refused' -and $u1.payload.remedy -like '*echo*' -and `
        $u1.payload.remedy -like '*boom*'
}
Check '7. `help` 是分章的（渐进披露的第二层）' {
    $m1.payload.effect -eq 'confirmed' -and `
        (@($m1.payload.data.sections | ForEach-Object { $_.id }) -contains 'whitelist')
}
Check '8. **能力校验在服务侧**：零能力的那个智能体被桥自己挡下（缺 `mcp.call`）' {
    $refus = @($records | Where-Object { $_.from -eq 'mcp:1' -and $_.to -eq 'agent:no' -and `
        $_.payload.effect -eq 'refused' })
    $refus.Count -ge 1 -and $refus[0].payload.why -like '*mcp.call*'
}
Check '9. 而签了 `mcp.call` 的那个**同一条调用是成功的**（差别只在凭证）' {
    @($records | Where-Object { $_.from -eq 'mcp:1' -and $_.to -eq 'agent:yes' -and `
        $_.payload.effect -eq 'confirmed' -and $_.payload.output -like '*从 MCP 回来的话*' }).Count -ge 1
}
Check '10. **对端死了**：桥报 offline 走了，中控那条现成的路接管（结未决调用 + 敕令）' {
    # 第一版这条判据写成 `$c1 -eq $null`（"调用方什么都收不到"）—— **写反了**。
    # 正确行为是：桥**不自己编答复**，而中控**必须给调用方一个交代**
    # （"不静默丢弃"那条纪律）。所以这里要查的是那份交代长什么样。
    $inject = @($records | Where-Object { $_.type -eq 'inject' -and $_.payload.reason -eq 'service_down' })
    $c1.payload.code -eq 'service_down' -and $c1.payload.effect -eq 'unverifiable' -and `
        $errText -like '*对端没了*' -and `
        -not (@($l2.payload.data.instances | ForEach-Object { $_.id }) -contains 'mcp:1') -and `
        $inject.Count -ge 1 -and $inject[0].payload.text -like '*我是**桥***'
}
Check '11. 这一路没有任何一条结果**违反自己声明的形**' {
    @($records | Where-Object { $_.payload.schema_violation }).Count -eq 0
}

Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    Write-Host '== stderr ==' -ForegroundColor DarkGray
    foreach ($l in ($errText -split "`n")) { if ($l.Trim() -ne '') { Write-Host "  $l" -ForegroundColor DarkGray } }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "工作目录留在：$work"
