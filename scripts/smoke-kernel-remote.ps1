# **第二个内核：跨进程那一档**（2026-09-26）。
#
#   pwsh -File scripts/smoke-kernel-remote.ps1
#
# 第 2 步把"内核"抽成了一层（`crates/harness-agent/src/kernel.rs`）；
# 这一份验**第 3 步**：把内核换成**另一个进程**（这里是 Python 写的大眼适配器），
# 智能体该怎么跟它说话、那一格 `sees_content` 是不是**真的门**。
#
# 它要证五件事：
#   1. **大眼那份记忆**（Python + SQLite）接起来了，而且库落在我们指定的地方；
#   2. **智能体真的在问它**（每个回合开头的 `recall`）；
#   3. **捞回来的东西进了上下文**，而且**记账了**（日志里那条 `kind=recalled`）；
#   4. **`sees_content: false` 是真的门**：那一档下连"查询"都不发（查询本身就是内容）；
#   5. **`review` 那一手是大眼的路子**：要裁掉的**沉淀进记忆库**，不是丢掉。
#
# ⚠️ **跑不起来就明确跳过**（同"没 key 就跳过"那条）：需要 `python` + 一个 bigeye 目录。
# **跳过不是通过** —— 它会说话。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$bigeye = Join-Path (Split-Path -Parent $root) 'bigeye-main'
if (-not (Get-Command python -ErrorAction SilentlyContinue)) {
    Write-Host '跳过：这个环境里没有 `python`（大眼适配器是 Python 写的）' -ForegroundColor Yellow
    exit 0
}
if (-not (Test-Path (Join-Path $bigeye 'memory\fragment_store.py'))) {
    Write-Host "跳过：找不到大眼的记忆模块（$bigeye）" -ForegroundColor Yellow
    exit 0
}
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$agentBin = Join-Path $root 'target\debug\harness-agent.exe'
$adapter = Join-Path $root 'tools\bigeye-kernel\kernel_service.py'
foreach ($b in @($ctrl, $agentBin, $adapter)) { if (-not (Test-Path $b)) { throw "产物不在：$b" } }
function J([string]$p) { return ($p -replace '\\', '\\') }
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-krem-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $work -Force | Out-Null
$db = Join-Path $work 'mem.sqlite'
$sid = 's-krem'
$script1 = Join-Path $work 'script.ndjson'
Set-Content -LiteralPath $script1 -Value '{"step":"text","text":"我看看。"}' -Encoding utf8

# ---- 0. 先往记忆库里放几条"它以前学过的"（记忆库比会话长命 —— 这就是那个形状）----
Write-Host '== 先给记忆库几条它以前学过的 ==' -ForegroundColor Cyan
$seedTexts = @(
    'ConPTY 起会话时 HPCON 要传值本身，不是指向它的指针',
    'CreateProcessW 的 cwd 不吃 \\?\ 前缀',
    '尾部半残的日志要能重建',
    '名字的水位由中控维护',
    '权限矩阵按会不会改状态分档',
    '只准降不准升'
)
$seedItems = @(); for ($i = 0; $i -lt $seedTexts.Count; $i++) {
    $seedItems += [ordered]@{ index = $i; kind = 'user'; text = $seedTexts[$i] }
}
# ⚠️ **用量要给到它阈值之上**（大眼那边默认 `compress_token_limit = 200000`）：
# 我们这一版是**照搬它的判断** —— 它说没满就不裁（下面第 7 条专门盯那条路）。
$seedMsg = [ordered]@{
    id = 'seed-1'; ts = 0; type = 'request'; from = 'kernel:1'; to = 'kernel:1'
    corr = 'seed'; sid = 'seed'; authority = 'observe'
    payload = @{ tool = 'review'; args = @{ window = 1; used_tokens = 250000; sees_content = $true; items = $seedItems } }
}
$seedOut = ($seedMsg | ConvertTo-Json -Compress -Depth 12) | python $adapter --bigeye $bigeye --db $db 2>&1
Check '1. 大眼适配器能起、能沉淀（记忆库落在我们指定的那个 sqlite 上）' {
    (Test-Path -LiteralPath $db) -and (($seedOut -join "`n") -like '*沉淀*')
}

function Run-Round([string]$tag, [bool]$sees, [string]$task) {
    $cfg = Join-Path $work "$tag.json"
    $seeJson = if ($sees) { 'true' } else { 'false' }
    @"
{
  "persona": { "name": "内核验收", "version": "1", "brief": "跑内核验收的那位" },
  "model":   { "kind": "mock" },
  "kernel":  { "kind": "remote", "service": "kernel:1", "sees_content": $seeJson, "window": 6000 }
}
"@ | Set-Content -LiteralPath $cfg -Encoding utf8
    $services = Join-Path $work "$tag-services.json"
    @"
[
  { "kind": "kernel", "id": "kernel:1", "role": "tool",
    "cmd": ["python", "$(J $adapter)"], "args": ["--bigeye", "$(J $bigeye)", "--db", "$(J $db)"] },
  { "kind": "agent", "id": "agent:dada", "role": "agent", "bindings": ["kernel:1"],
    "cmd": ["$(J $agentBin)"],
    "args": ["--model", "mock", "--script", "$(J $script1)", "--config", "$(J $cfg)"] }
]
"@ | Set-Content -LiteralPath $services -Encoding utf8
    $dataDir = Join-Path $work "data-$tag"
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $ctrl
    $psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$services`" --drain-ms 3000 --tick-ms 0"
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
    $proc = [System.Diagnostics.Process]::Start($psi)
    Start-Sleep -Seconds 3
    # **派一件活**：这一件活会走"回合开头问内核要回忆"那条路
    $req = [ordered]@{ id = 'm-task'; ts = 0; type = 'request'; from = 'console'; to = 'agent:dada'
                       corr = "c-$tag"; sid = $sid; authority = 'user'; payload = @{ task = $task } }
    $proc.StandardInput.WriteLine(($req | ConvertTo-Json -Compress -Depth 12))
    $proc.StandardInput.Flush()
    $deadline = [DateTime]::UtcNow.AddSeconds(25)
    $done = $false
    # ⚠️ **同一个流上只能挂一个 `ReadLineAsync`** —— 上一个没读完就再挂，
    # 会直接抛 "The stream is currently in use by a previous operation"。
    # 所以挂着的那个任务要**留着**，下一轮循环接着等它（同 `smoke-resume` 那条注释）。
    $pending = $null
    while ([DateTime]::UtcNow -lt $deadline -and -not $done) {
        if ($null -eq $pending) { $pending = $proc.StandardOutput.ReadLineAsync() }
        if (-not $pending.Wait(5000)) { continue }
        $line = $pending.Result
        $pending = $null
        if ($line) {
            $m = $line | ConvertFrom-Json
            if ($m.type -eq 'result' -and $m.corr -eq "c-$tag") { $done = $true }
        }
    }
    $proc.StandardInput.Close()
    $proc.WaitForExit(8000) | Out-Null
    return @{ err = $proc.StandardError.ReadToEnd(); dataDir = $dataDir }
}

Write-Host '== 第 1 轮：sees_content = true（A 档）==' -ForegroundColor Cyan
$r1 = Run-Round 'a' $true 'ConPTY 那个坑再讲一遍'
$logPath = Join-Path (Join-Path (Join-Path $r1.dataDir 'sessions') $sid) 'log.jsonl'
$recs = @(Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } |
    ForEach-Object { $_ | ConvertFrom-Json })
$recalled = @($recs | Where-Object { $_.payload.kind -eq 'model_item' -and $_.payload.item.kind -eq 'recalled' })
Check '2. **智能体真的在问它**：内核那边收到了 `recall`（它 stderr 里那一行）' {
    $r1.err -like '*recall*'
}
Check '3. **捞回来的东西进了上下文，而且记账了**（日志里那条 `kind=recalled`）' {
    $recalled.Count -ge 1 -and $recalled[0].payload.item.text -like '*【回忆】*'
}
Check '4. 捞回来的是**大眼库里那条**（不是编的）：带着库里那段话的特征词' {
    $recalled.Count -ge 1 -and $recalled[0].payload.item.text -like '*HPCON*'
}

Write-Host '== 第 2 轮：sees_content = false（B 档）==' -ForegroundColor Cyan
$r2 = Run-Round 'b' $false 'ConPTY 那个坑再讲一遍'
Check '5. **那一格是真的门**：不看正文时**连查询都不发** —— 内核一次 `recall` 都没收到' {
    $r2.err -notlike '*recall*' -and $r2.err -like '*不看正文*'
}
Check '6. `review` 把要裁掉的**沉淀进记忆库**，而不是丢掉（签名里说得出沉淀了几条）' {
    ($seedOut -join "`n") -like '*沉淀 5 条*'
}
# 7. **满没满是它的判断，不是我们的**：喂一个低于它阈值的用量 ⇒ 它回"没满，不裁"。
#    （阈值在它库里的 `meta.compress_token_limit`；我们只是把真实用量喂进它留的那个口子。）
$calmMsg = [ordered]@{
    id = 'calm-1'; ts = 0; type = 'request'; from = 'kernel:1'; to = 'kernel:1'
    corr = 'calm'; sid = 'seed'; authority = 'observe'
    payload = @{ tool = 'review'; args = @{ window = 1; used_tokens = 1000; sees_content = $true; items = $seedItems } }
}
$calmOut = ($calmMsg | ConvertTo-Json -Compress -Depth 12) | python $adapter --bigeye $bigeye --db $db 2>&1
Check '7. **它说没满就不裁**（阈值是它的，我们只是把真实用量喂进去）' {
    ($calmOut -join "`n") -like '*不裁*'
}
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    Write-Host '== 第 1 轮 stderr ==' -ForegroundColor DarkGray
    $r1.err -split "`n" | ForEach-Object { if ($_.Trim()) { Write-Host ('  ' + $_) -ForegroundColor DarkGray } }
    Write-Host '== 第 2 轮 stderr ==' -ForegroundColor DarkGray
    $r2.err -split "`n" | ForEach-Object { if ($_.Trim()) { Write-Host ('  ' + $_) -ForegroundColor DarkGray } }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host ("工作目录留在：" + $work) -ForegroundColor DarkGray
