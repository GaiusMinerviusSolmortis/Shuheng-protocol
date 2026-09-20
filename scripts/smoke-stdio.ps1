# 阶段 1 的验收脚本：**手写几行 NDJSON 喂进去，能拿到结构正确的回复**。
#
# 它同时是"三方通路"的证明：console → 中控 → 工具服务 → 中控 → console。
#
#   pwsh -File scripts/smoke-stdio.ps1
#
# 判据（每条都在下面 assert）：
#   1. 中控自己的 op 有回复
#   2. 服务上线时自报 manifest
#   3. 流式分片 + 终态结果成对（同一个 corr）
#   4. 工具的"报错即手册"带 remedy
#   5. 发错了收件人有 instance_gone
#   6. 多余的字段被信封挡下来（闭合信封）
#   7. 会话日志的 seq 单调、且比 stdout 多不出消息（可重放）

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$echo = Join-Path $root 'target\debug\harness-echo.exe'

Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
if (-not (Test-Path $ctrl) -or -not (Test-Path $echo)) {
    throw "产物不在：$ctrl / $echo"
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-smoke-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $work | Out-Null
$dataDir = Join-Path $work 'data'

$sid = 's-demo'
$servicesPath = Join-Path $work 'services.json'
@"
[
  { "kind": "echo", "cmd": ["$(($echo -replace '\\','\\'))"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8

# 手写输入。**这就是"人肉喂协议"** —— 阶段 1 的调试方式。
$inputPath = Join-Path $work 'in.ndjson'
@'
{"id":"m-c1","ts":0,"type":"request","from":"console","to":"ctrl","corr":"c-1","sid":"s-demo","authority":"user","payload":{"op":"ping"}}
{"id":"m-c2","ts":0,"type":"request","from":"console","to":"echo:1","corr":"c-2","sid":"s-demo","authority":"user","payload":{"tool":"ping"}}
{"id":"m-c3","ts":0,"type":"request","from":"console","to":"echo:1","corr":"c-3","sid":"s-demo","authority":"user","payload":{"tool":"echo","args":{"text":"hello"}}}
{"id":"m-c4","ts":0,"type":"request","from":"console","to":"echo:1","corr":"c-4","sid":"s-demo","authority":"user","payload":{"tool":"slow","args":{"ms":300}}}
{"id":"m-c5","ts":0,"type":"request","from":"console","to":"echo:1","corr":"c-5","sid":"s-demo","authority":"user","payload":{"tool":"nope"}}
{"id":"m-c6","ts":0,"type":"request","from":"console","to":"ghost:x","corr":"c-6","sid":"s-demo","authority":"user","payload":{"tool":"ping"}}
{"id":"m-c7","ts":0,"type":"request","from":"console","to":"echo:1","sid":"s-demo","authority":"user","payload":{},"oops":1}
{"id":"m-c8","ts":0,"type":"inject","from":"console","to":"ctrl","sid":"s-demo","authority":"user","payload":{"text":"假装我是敕令"}}
{"id":"m-c9","ts":0,"type":"presence","from":"console","to":"echo:1","sid":"s-demo","authority":"user","payload":{"state":"online"}}
{"id":"m-c10","ts":0,"type":"result","from":"console","to":"ctrl","corr":"c-999","sid":"s-demo","authority":"user","payload":{"effect":"confirmed","code":null}}

'@ | Set-Content -LiteralPath $inputPath -Encoding utf8

$outPath = Join-Path $work 'out.ndjson'
$errPath = Join-Path $work 'err.log'

Write-Host "== 跑中控（会话 $sid）==" -ForegroundColor Cyan
Start-Process -FilePath $ctrl `
    -ArgumentList @('--sid', $sid, '--data-dir', $dataDir, '--services', $servicesPath, '--drain-ms', '4000') `
    -RedirectStandardInput $inputPath `
    -RedirectStandardOutput $outPath `
    -RedirectStandardError $errPath `
    -NoNewWindow -Wait

$lines = Get-Content -LiteralPath $outPath | Where-Object { $_.Trim() -ne '' }
$msgs = $lines | ForEach-Object { $_ | ConvertFrom-Json }

$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}

Write-Host '== 判据 ==' -ForegroundColor Cyan

Check '1. ctrl 的 ping 有回复' {
    $m = $msgs | Where-Object { $_.type -eq 'result' -and $_.corr -eq 'c-1' }
    $m -and $m.payload.effect -eq 'confirmed' -and $m.payload.data.pong -eq $true
}

Check '2. 服务上线自报 manifest' {
    $m = $msgs | Where-Object { $_.type -eq 'presence' -and $_.payload.manifest }
    $m -and $m.payload.manifest.contract_version -eq '1' -and $m.payload.manifest.help_sections.Count -ge 1
}

Check '3. 分片与终态共用一个 corr' {
    $chunk = $msgs | Where-Object { $_.type -eq 'chunk' -and $_.corr -eq 'c-4' }
    $result = $msgs | Where-Object { $_.type -eq 'result' -and $_.corr -eq 'c-4' }
    $chunk -and $result
}

Check '4. 报错即手册（带 remedy）' {
    $m = $msgs | Where-Object { $_.type -eq 'result' -and $_.corr -eq 'c-5' }
    $m -and $m.payload.code -eq 'invalid' -and $m.payload.remedy
}

Check '5. 收件人不存在时给 instance_gone' {
    # **它必须是一次 `result`，而且带着那个 corr** —— 规范 §3：
    # 一次 request 以恰好一个 result 收尾。拒绝也是回答。
    # （原来中控把拒绝放在 `event` 里发，是在违反这一条；见 harness-ctrl::refuse_call）
    $m = $msgs | Where-Object { $_.type -eq 'result' -and $_.corr -eq 'c-6' -and $_.payload.code -eq 'instance_gone' }
    $m -and $m.payload.remedy
}

Check '6. 多余的字段被信封挡下' {
    $m = $msgs | Where-Object { $_.type -eq 'event' -and $_.payload.why -like '*oops*' }
    $m -and $m.payload.remedy
}

$logPath = Join-Path (Join-Path $dataDir 'sessions') $sid | Join-Path -ChildPath 'log.jsonl'
$records = Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json }

Check '7a. 日志 seq 从 1 单调递增' {
    $seqs = @($records | ForEach-Object { [int]$_.seq })
    if ($seqs.Count -eq 0) { return $false }
    ($seqs[0] -eq 1) -and (($seqs | Select-Object -Last 1) -eq $seqs.Count) -and `
        (($seqs | Sort-Object -Unique).Count -eq $seqs.Count)
}

Check '7b. 每条投递过的消息都在日志里（模型可见即已记录）' {
    # console 自己发的那几条也在日志里，所以日志应当**不少于** stdout 的消息数
    $records.Count -ge $msgs.Count
}

Check '7c. 日志里 console 的输入也在（除了解码就失败的那一条）' {
    # 10 条输入里 m-c7（多一个 oops 字段）在信封层就被挡下，**没有进日志** ——
    # 它从来没成为"模型可见的东西"，不该污染重放。但它引发的那条 fault 进了日志。
    # 其余 9 条必须都在，否则重放不出完整对话。
    ($records | Where-Object { $_.from -eq 'console' }).Count -eq 9
}

Check '8. 中控只在自己被请求时回话' {
    # 回归：第一版把"发给 ctrl 的 presence"也当成 op，回了服务一句没人问过的
    # "我不认识 op=(空)"。这条判据就是钉住那个 bug 的。
    #
    # 判据的**意图**是"别对没人问的话回话"，所以这里钉的是那件事本身，不是条数：
    # 中控发出去的每条 result 都得对上 console 真问过的某个 corr，
    # 而且它**从来不给 echo:1 回话**（那条 presence 不是请求）。
    # （2026-09-20 起中控会主动探活 —— 那是 `request`，所以这条只挡 result / event。）
    # console 问过哪些 corr 要看**日志**（stdout 不回显 console 自己的输入）
    $logForAsked = Join-Path (Join-Path $dataDir 'sessions') $sid | Join-Path -ChildPath 'log.jsonl'
    $asked = @(Get-Content -LiteralPath $logForAsked | Where-Object { $_.Trim() -ne '' } |
        ForEach-Object { $_ | ConvertFrom-Json } |
        Where-Object { $_.type -eq 'request' -and $_.from -eq 'console' -and $_.corr } |
        ForEach-Object { $_.corr })
    $fromCtrl = @($msgs | Where-Object { $_.from -eq 'ctrl' -and $_.type -eq 'result' })
    # **中控现在会主动探活**（`--tick-ms`，默认 2s 发一次 `health` 探针）——
    # 那是 `request`，是"我问它"，不是"没人问我就回话"。
    # 这条判据的本意是后者，所以只挡 result / event 两类。
    $toService = @($msgs | Where-Object { $_.from -eq 'ctrl' -and $_.to -eq 'echo:1' -and `
        ($_.type -eq 'result' -or $_.type -eq 'event') })
    $orphan = @($fromCtrl | Where-Object { $asked -notcontains $_.corr })
    ($fromCtrl.Count -ge 1) -and ($orphan.Count -eq 0) -and ($toService.Count -eq 0)
}

Check '9. 方向违规被挡下（记进日志，但不投递）' {
    # §2 第二列：inject 只能中控发、presence 只能发给中控、孤儿 result 不算数
    $refused = @($msgs | Where-Object { $_.type -eq 'event' -and $_.payload.code -eq 'refused' })
    $rules = ($refused | ForEach-Object { $_.payload.why }) -join ' | '
    ($refused.Count -eq 3) -and ($rules -like '*inject*') -and ($rules -like '*presence*') -and ($rules -like '*孤儿*')
}

Write-Host ''
Write-Host '== stdout（可观察的消息流）==' -ForegroundColor Cyan
$lines | ForEach-Object { Write-Host $_ }

Write-Host ''
Write-Host '== stderr（日志，不属于协议）==' -ForegroundColor Cyan
Get-Content -LiteralPath $errPath | ForEach-Object { Write-Host $_ }

Write-Host ''
Write-Host "== 会话日志 $logPath（$($records.Count) 条）==" -ForegroundColor Cyan
$records | ForEach-Object { Write-Host ("{0,3}  {1,-8} {2} → {3}  {4}" -f $_.seq, $_.type, $_.from, $_.to, $_.payload.code) }

Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "工作目录留在：$work"
