# 落盘屏障第三个边界：**模型请求前**（规范-消息协议 §5.5 #8）。
#
#   pwsh -File scripts/smoke-barrier.ps1
#
# 三个边界，前两个早就有调用点了（收摊 / 有副作用工具分派前）。
# 第三个本来**连路都没有**：会话日志是**中控独占**的，而"我要去问模型了"
# 这件事只有**智能体**知道 —— 它手里没有那支笔，也没有能用的 op。
#
# 所以这一条补的是两样：
#   1. 中控开一个 `barrier` op（谁需要谁喊，中控负责 flush + fsync）；
#   2. 智能体在每一次 `model.reply()` 之前喊一声。
#
# 它防的是：模型见过的东西要能从日志重建（不变量二）。日志还压在写缓冲里
# 就去问模型 —— 崩在中间，重放出来的历史和模型当时看到的**不是同一份**。
#
# 顺带钉住三件"顺手会做错"的事：
#   * 屏障**不占模型的步数**（它不是一次工具调用，不该把 trace.steps 算多）；
#   * `synced` 是**诚实的**（本来就干净时说 false，不装作落了盘）；
#   * 落不成**不拦路**（它是审计护栏，不是"这一步能不能做"的门）。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$echo = Join-Path $root 'target\debug\harness-echo.exe'
$agentBin = Join-Path $root 'target\debug\harness-agent.exe'
foreach ($b in @($ctrl, $echo, $agentBin)) { if (-not (Test-Path $b)) { throw "产物不在：$b" } }

function J([string]$p) { return ($p -replace '\\', '\\') }
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-barrier-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null
$sid = 's-barrier'
$dataDir = Join-Path $work 'data'
# 两步：先调一次工具（这一步模型要"说话"），再给结论 —— 于是 model.reply 会被调两次
$scriptPath = Join-Path $work 'script.ndjson'
@'
{"step":"tool","calls":[{"to":"echo:1","tool":"echo","args":{"text":"BARRIER-STEP"}}]}
{"step":"text","text":"两步都干完了。"}
'@ | Set-Content -LiteralPath $scriptPath -Encoding utf8
$servicesPath = Join-Path $work 'services.json'
@"
[
  { "kind": "echo", "id": "echo:1", "role": "tool", "cmd": ["$(J $echo)"] },
  { "kind": "agent", "id": "agent:dada", "role": "agent", "bindings": ["echo:1"],
    "cmd": ["$(J $agentBin)"],
    "args": ["--persona", "达达", "--persona-version", "1",
             "--fence", "$(J $project)",
             "--script", "$(J $scriptPath)",
             "--model-window", "8000"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ctrl
# `--debug-barriers` 让"这一下真的发生了"看得见（正常路径不出声）
$psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 8000 --tick-ms 0 --debug-barriers"
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
function Write-Env([string]$to, [string]$corr, [hashtable]$payload) {
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

# ---- ① 先直接问中控一次：这个 op 存在吗、它答得诚实吗 ----
Write-Env 'ctrl' 'b-direct' @{ op = 'barrier'; reason = '控制台直接试一次' }
$direct = Wait 'b-direct'

# ---- ② 再让智能体自己跑一个两步的回合 ----
Write-Env 'agent:dada' 'c-task' @{ task = '先让 echo 说一句话，然后告诉我做完了' }
$ra = Wait 'c-task' 40000

$proc.StandardInput.Close()
$proc.WaitForExit(8000) | Out-Null
try {
    $drain = $proc.StandardOutput.ReadToEndAsync(); $drain.Wait(4000) | Out-Null
} catch { }
$et = $proc.StandardError.ReadToEndAsync(); $et.Wait(4000) | Out-Null
$errText = if ($et.IsCompleted) { $et.Result } else { '' }
$logPath = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl'
$records = @(Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json })

$barriers = @($records | Where-Object { $_.type -eq 'request' -and $_.payload.op -eq 'barrier' -and $_.from -eq 'agent:dada' })
$barrierCorrs = @($barriers | ForEach-Object { $_.corr })
$barrierResults = @($records | Where-Object { $_.type -eq 'result' -and $barrierCorrs -contains $_.corr })
$assistantItems = @($records | Where-Object { $_.payload.kind -eq 'model_item' -and $_.payload.item.kind -eq 'assistant' })

Write-Host '== 判据 ==' -ForegroundColor Cyan
Check '1. 中控真的有 `barrier` 这个 op，而且答的是"落了没有"（不是空话）' {
    $direct -and $direct.payload.effect -eq 'confirmed' -and `
        ($direct.payload.data.PSObject.Properties.Name -contains 'synced') -and `
        ($direct.payload.data.synced -is [bool]) -and $direct.payload.data.last_seq -ge 1
}
Check '2. **智能体每一步都喊了一声**（两步 ⇒ 两条 barrier 请求，reason 是它自己写的）' {
    $barriers.Count -eq 2 -and (($barriers | Where-Object { $_.payload.reason -eq '模型请求前' }).Count -eq 2)
}
Check '3. 每一条 barrier 都有结果收尾（不许挂在那儿没人回）' {
    $barrierResults.Count -eq $barriers.Count -and `
        (@($barrierResults | Where-Object { $_.payload.effect -ne 'confirmed' }).Count -eq 0)
}
Check '4. **顺序对**：每一次模型开口之前，都先落了盘' {
    $bseq = @($barriers | ForEach-Object { $_.seq })
    $aseq = @($assistantItems | ForEach-Object { $_.seq })
    ($bseq.Count -eq 2) -and ($aseq.Count -eq 2) -and `
        ($bseq[0] -lt $aseq[0]) -and ($bseq[1] -lt $aseq[1])
}
Check '5. **至少有一次是真的 fsync**（`synced: true`）—— 不是空转' {
    @($barrierResults | Where-Object { $_.payload.data.synced -eq $true }).Count -ge 1
}
Check '6. 屏障**不占模型的步数**：trace 上还是两步、一次工具调用' {
    $ra.payload.trace.steps -eq 2 -and $ra.payload.trace.tool_calls -eq 1
}
Check '7. **`--debug-barriers` 下看得见**这行（机制可观察，不是"我说它跑了"）' {
    $errText -like '*屏障：模型请求前*'
}
Check '8. 屏障**先于**工具的调用到达服务（落盘在"动手"之前，不是之后补的）' {
    $toolCall = @($barriers | Select-Object -First 1).seq
    $echoReq = @($records | Where-Object { $_.type -eq 'request' -and $_.to -eq 'echo:1' -and $_.payload.tool -eq 'echo' } | Select-Object -First 1).seq
    $toolCall -and $echoReq -and ($toolCall -lt $echoReq)
}
Check '9. **派活给智能体不算"有副作用的工具分派"**（别为它白落一次盘）' {
    # 智能体不是工具，没有 `destructiveHint` 可读 —— 对它就**不该走**那条
    # "读不到声明就保守落一次"。判据：stderr 里没有那句会误导人的话。
    ($errText -notlike '*读不到它的声明*')
}

Write-Host ''
Write-Host '== 中控 stderr（屏障那几行）==' -ForegroundColor Cyan
foreach ($l in ($errText -split "`n")) { if ($l -like '*屏障*') { Write-Host "  $($l.Trim())" } }
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "工作目录留在：$work"
