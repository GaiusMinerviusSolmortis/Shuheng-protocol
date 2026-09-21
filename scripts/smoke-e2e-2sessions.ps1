# **两个会话：靠记忆把话接下去**（2026-09-20）。
#
#   pwsh -File scripts/smoke-e2e-2sessions.ps1
#
# 上一条整机验收（`smoke-e2e-agent`）跑的是"一个会话里把一件活干完"。
# 这一条跑的是**下一件事**：**会话是会结束的，而事情不会** ——
# 第一个会话把结论**沉淀**到记忆服务，第二个会话（**换一个 data 目录**，
# 真的重开一次）要**把上一次的结论取回来**。
#
# 它一次压到四样零件：**记忆服务**（锚点/碎片）、**跨会话**、
# **能力签发**（`mem.write` / `mem.read` 各签一次）、**真模型**。
# 少任何一样都接不上：没沉淀第二段就无从取；没能力它会被自己人挡下；
# 跨会话没做对它读的是另一个库。
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
$mem = Join-Path $bin 'harness-mem.exe'
$editor = Join-Path $bin 'harness-editor.exe'
$agentBin = Join-Path $bin 'harness-agent.exe'
foreach ($b in @($ctrl, $mem, $editor, $agentBin)) { if (-not (Test-Path $b)) { throw "产物不在：$b" } }
function J([string]$p) { return ($p -replace '\\', '\\') }
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-e2e2-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null
Set-Content -LiteralPath (Join-Path $project 'app.txt') -Value "name=demo`nversion=1`n" -Encoding utf8 -NoNewline
# **一个碎片库，两个会话共用** —— 这正是"记忆活着、会话短命"的样子
$store = Join-Path $work 'memory'

function Start-Session([string]$sid, [string]$tag, [string]$task, [int]$waitSec) {
    $dataDir = Join-Path $work "data-$tag"
    $servicesPath = Join-Path $work "services-$tag.json"
    @"
[
  { "kind": "mem", "id": "mem:1", "role": "tool",
    "cmd": ["$(J $mem)"], "args": ["--store", "$(J $store)", "--default-ttl-ms", "0"] },
  { "kind": "editor", "id": "editor:1", "role": "tool",
    "cmd": ["$(J $editor)"], "args": ["--fence", "$(J $project)"] },
  { "kind": "agent", "id": "agent:dada", "role": "agent", "bindings": ["mem:1", "editor:1"],
    "cmd": ["$(J $agentBin)"],
    "args": ["--model", "deepseek", "--base-url", "$BaseUrl", "--model-name", "$ModelName",
             "--persona", "达达", "--fence", "$(J $project)",
             "--model-window", "32000", "--max-steps", "12", "--max-tokens", "1200"] }
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
    $outTask = $null
    $seen = New-Object System.Collections.Generic.List[string]
    # 发那条令
    $msg = [ordered]@{
        id = 'm-1'; ts = 0; type = 'request'; from = 'console'; to = 'agent:dada'
        corr = 'c-1'; sid = $sid; authority = 'user'; payload = @{ task = $task }
    }
    $proc.StandardInput.WriteLine(($msg | ConvertTo-Json -Compress -Depth 12))
    $proc.StandardInput.Flush()
    $deadline = [DateTime]::UtcNow.AddSeconds($waitSec)
    $answer = $null
    while ([DateTime]::UtcNow -lt $deadline) {
        if ($null -eq $outTask) { $outTask = $proc.StandardOutput.ReadLineAsync() }
        $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
        if (-not $outTask.Wait($left)) { continue }
        $line = $outTask.Result
        $outTask = $null
        if ($null -eq $line) { break }
        if ($line.Trim() -eq '') { continue }
        $seen.Add($line) | Out-Null
        $m = $line | ConvertFrom-Json
        if ($m.type -eq 'result' -and $m.corr -eq 'c-1') { $answer = $m; break }
    }
    $proc.StandardInput.Close()
    $proc.WaitForExit(20000) | Out-Null
    $drain = $proc.StandardOutput.ReadToEndAsync(); $drain.Wait(5000) | Out-Null
    if ($drain.IsCompleted -and $drain.Result) {
        foreach ($l in ($drain.Result -split "`n")) { if ($l.Trim() -ne '') { $seen.Add($l.Trim()) | Out-Null } }
    }
    $et = $proc.StandardError.ReadToEndAsync(); $et.Wait(5000) | Out-Null
    $logPath = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl'
    $records = @()
    if (Test-Path $logPath) {
        $records = @(Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json })
    }
    return [pscustomobject]@{ answer = $answer; records = $records; seen = $seen }
}

# ---- 第一个会话：干一件活，然后**把结论沉淀下来** ----
$task1 = "请做两件事：① 用编辑器把 app.txt 里 version=1 改成 version=2；" +
         "② 然后用记忆服务的 deposit，把一句结论沉淀到锚点 `"$project`" 上" +
         '（内容要写清「把这个项目的 app.txt 从 version=1 改成了 version=2」）。' +
         "做完用一句话告诉我你做了什么。"
Write-Host '== 会话一：干活 + 沉淀 ==' -ForegroundColor Cyan
$one = Start-Session 's-one' 'one' $task1 180
$calls1 = @($one.records | Where-Object { $_.type -eq 'request' -and $_.from -eq 'agent:dada' -and $_.payload.tool })
Write-Host ('  它调了：' + (($calls1 | ForEach-Object { $_.to + '/' + $_.payload.tool }) -join '  ')) -ForegroundColor DarkGray
if ($one.answer) { Write-Host ('  结论：' + $one.answer.payload.text) -ForegroundColor DarkGray }

Check '1. 会话一：模型给了结论（不是"模型这条路断了"）' {
    $null -ne $one.answer -and $one.answer.payload.effect -eq 'confirmed' -and $one.answer.payload.text.Length -gt 0
}
Check '2. 会话一：活干成了（磁盘上 version=2）' {
    (Get-Content -LiteralPath (Join-Path $project 'app.txt') -Raw) -like '*version=2*'
}
Check '3. 会话一：它**真的沉淀了**（mem 收到了 deposit，而且带了锚点）' {
    $dep = @($one.records | Where-Object { $_.type -eq 'request' -and $_.to -eq 'mem:1' -and $_.payload.tool -eq 'deposit' })
    $dep.Count -ge 1 -and (@($dep | Where-Object { $_.payload.args.anchor -eq $project }).Count -ge 1)
}
Check '3b. 会话一的历史里**没有我们自己拼的记号**（模型看到的必须是真正的 tool_calls）' {
    # 这条钉的是一个**把自己教坏**的 bug（2026-09-20）：智能体原来会给每次调用
    # 额外记一条 assistant 文本 `[调用] name(args)`，那条文本会被回放给模型当上文 ——
    # 于是模型学到"动手的方式是写这行文本"，第三轮就真的只写了文本、没发出调用。
    $mine = @($one.records |
        Where-Object { $_.payload.kind -eq 'model_item' -and $_.payload.item.kind -eq 'assistant' } |
        Where-Object { $_.payload.item.text -like '*[调用]*' })
    $mine.Count -eq 0
}

# ---- 第二个会话：**换一个 data 目录**（真的重开），只靠记忆接话 ----
$task2 = "上一个会话做了些事，我不记得了。请用记忆服务的 recall（锚点 `"$project`"）" +
         "把上次沉淀下来的结论取回来，用一句话复述给我。**不要**去改任何文件。"
Write-Host '== 会话二：只靠记忆接上 ==' -ForegroundColor Cyan
$two = Start-Session 's-two' 'two' $task2 180
$calls2 = @($two.records | Where-Object { $_.type -eq 'request' -and $_.from -eq 'agent:dada' -and $_.payload.tool })
Write-Host ('  它调了：' + (($calls2 | ForEach-Object { $_.to + '/' + $_.payload.tool }) -join '  ')) -ForegroundColor DarkGray
if ($two.answer) { Write-Host ('  结论：' + $two.answer.payload.text) -ForegroundColor DarkGray }

Check '4. 会话二：它去**取**了（recall 打到了 mem，带同一个锚点）' {
    $rc = @($two.records | Where-Object { $_.type -eq 'request' -and $_.to -eq 'mem:1' -and $_.payload.tool -eq 'recall' })
    $rc.Count -ge 1 -and (@($rc | Where-Object { $_.payload.args.anchor -eq $project }).Count -ge 1)
}
Check '5. **跨会话接上了**：第二个会话的结论里说出了上次那件事（version=1 → 2）' {
    $t = if ($two.answer) { [string]$two.answer.payload.text } else { '' }
    ($t -like '*version=2*' -or $t -like '*version 2*') -and ($t -like '*1*')
}
Check '6. 会话二**没去动文件**（它被要求只取不改）' {
    @($calls2 | Where-Object { $_.to -eq 'editor:1' -and $_.payload.tool -in @('edit', 'revert', 'restore') }).Count -eq 0
}
Check '7. 两个会话都没有 `service_down` / `schema_violation`' {
    $bad = @($one.records + $two.records | Where-Object { $_.payload.code -eq 'service_down' -or $_.payload.schema_violation })
    $bad.Count -eq 0
}

Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host ("碎片库留在：" + $store) -ForegroundColor DarkGray
