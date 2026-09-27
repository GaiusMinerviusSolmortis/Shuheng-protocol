# **契约闸门**：manifest 合不合规（注册时）＋结果合不合自己声明的形（投递前）。
#
#   pwsh -File scripts/smoke-contract.ps1
#
# 这一轮补的是规范 §11.1 C 组那笔欠账：`output_schema` / `lifecycle` /
# `idle_timeout` / `health` 原来**一个都没实现** —— "结果形状没人校验"。
#
# 两道闸门都在**中控**，理由和 §11.2 硬规则二那句一样：
# **验证是独立的一次调用** —— 让服务自己检查自己，等于让被验证的人写成绩单。
#
# 要验证"闸门真的会响"，就得有人**故意说谎**（这是 mock 存在的理由）：
#   * `echo:holes`  —— 家门少报一项（注册那道该拦）
#   * `echo:liar`   —— 家门报得齐，可做事不按自己说的来（投递那道该拦）
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$echo = Join-Path $root 'target\debug\harness-echo.exe'
$editor = Join-Path $root 'target\debug\harness-editor.exe'
function J([string]$p) { return ($p -replace '\\', '\\') }
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-contract-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null
$sid = 's-contract'
$dataDir = Join-Path $work 'data'
$servicesPath = Join-Path $work 'services.json'
@"
[
  { "kind": "echo",   "id": "echo:1",     "role": "tool", "cmd": ["$(J $echo)"] },
  { "kind": "echo",   "id": "echo:holes", "role": "tool", "cmd": ["$(J $echo)"], "args": ["--manifest-holes"] },
  { "kind": "echo",   "id": "echo:liar",  "role": "tool", "cmd": ["$(J $echo)"], "args": ["--bad-result"] },
  { "kind": "echo",   "id": "echo:sandbox-liar", "role": "tool", "cmd": ["$(J $echo)"], "args": ["--sandbox-lie"] },
  { "kind": "echo",   "id": "echo:state-liar",   "role": "tool", "cmd": ["$(J $echo)"], "args": ["--bad-state"] },
  { "kind": "echo",   "id": "echo:cap-liar",     "role": "tool", "cmd": ["$(J $echo)"], "args": ["--manifest-bad-caps"] },
  { "kind": "echo",   "id": "echo:future",       "role": "tool", "cmd": ["$(J $echo)"], "args": ["--contract-version", "2.0"] },
  { "kind": "echo",   "id": "echo:ancient",      "role": "tool", "cmd": ["$(J $echo)"], "args": ["--contract-version", "0.9"] },
  { "kind": "echo",   "id": "echo:extended",     "role": "tool", "cmd": ["$(J $echo)"], "args": ["--contract-version", "1.9"] },
  { "kind": "editor", "id": "editor:1",   "role": "tool",
    "cmd": ["$(J $editor)"], "args": ["--fence", "$(J $project)"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ctrl
$psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 4000"
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
function Send($payload, [string]$corr) {
    $m = [ordered]@{ id = "m-$corr"; ts = 0; type = 'request'; from = 'console'; to = $payload.to
                     corr = $corr; sid = $sid; authority = 'user'; payload = $payload.body }
    $proc.StandardInput.WriteLine(($m | ConvertTo-Json -Compress -Depth 12))
    $proc.StandardInput.Flush()
}
$replies = @{}
function Pump([int]$timeoutMs, [scriptblock]$done) {
    $deadline = [DateTime]::UtcNow.AddMilliseconds($timeoutMs)
    while ([DateTime]::UtcNow -lt $deadline) {
        $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
        $line = NextLine $left
        if ($null -eq $line) { continue }
        if ($line.Trim() -eq '') { continue }
        $m = $line | ConvertFrom-Json
        if ($m.type -eq 'result' -and $m.corr) { $replies[$m.corr] = $m }
        if (& $done) { return $true }
    }
    return $false
}
# **先等所有服务上线，再派活。** 反过来的话，第一条请求会撞上一个
# 还没注册的收件人 —— 那时中控回 `instance_gone` 是**诚实的**，
# 但它证明不了"家门不合规会被挡下"这件事。测法要配得上被测的东西。
$ready = $false
for ($i = 0; $i -lt 25 -and -not $ready; $i++) {
    $corr = "c-ready-$i"
    Send @{ to = 'ctrl'; body = @{ op = 'list' } } $corr
    Pump 500 { $replies.ContainsKey($corr) } | Out-Null
    if ($replies.ContainsKey($corr)) {
        $ids = @($replies[$corr].payload.data.instances | ForEach-Object { $_.id })
        $ready = ($ids -contains 'echo:1') -and ($ids -contains 'echo:holes') -and
                 ($ids -contains 'echo:liar') -and ($ids -contains 'editor:1') -and
                 ($ids -contains 'echo:extended')
    }
    if (-not $ready) { Start-Sleep -Milliseconds 200 }
}
if (-not $ready) { Write-Host '服务没能全部上线，后面的判据会失真' -ForegroundColor Yellow }
Send @{ to = 'echo:1';     body = @{ tool = 'echo'; args = @{ text = '正常的那个' } } } 'c-good'
Send @{ to = 'echo:holes'; body = @{ tool = 'ping'; args = @{} } }                   'c-holes'
Send @{ to = 'echo:liar';  body = @{ tool = 'echo'; args = @{ text = '说谎的那个' } } } 'c-liar'
Send @{ to = 'echo:state-liar'; body = @{ tool = 'ping'; args = @{} } }            'c-state'
Send @{ to = 'echo:extended'; body = @{ tool = 'echo'; args = @{ text = '带扩展的那个' } } } 'c-ext'
Send @{ to = 'echo:future';   body = @{ tool = 'ping'; args = @{} } }              'c-future'
Send @{ to = 'echo:ancient';  body = @{ tool = 'ping'; args = @{} } }              'c-ancient'
Send @{ to = 'ctrl';       body = @{ op = 'list' } }                                 'c-list'
Pump 25000 {
    $replies.ContainsKey('c-good') -and $replies.ContainsKey('c-holes') -and
    $replies.ContainsKey('c-liar') -and $replies.ContainsKey('c-list') -and
    $replies.ContainsKey('c-state') -and $replies.ContainsKey('c-ext') -and
    $replies.ContainsKey('c-future') -and $replies.ContainsKey('c-ancient')
} | Out-Null
$proc.StandardInput.Close()
$proc.WaitForExit(8000) | Out-Null
$et = $proc.StandardError.ReadToEndAsync(); $et.Wait(4000) | Out-Null
$logPath = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl'
$records = @(Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json })

$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
$nonconforming = @($records | Where-Object { $_.payload.code -eq 'manifest_nonconforming' })
$violationEvents = @($records | Where-Object { $_.payload.code -eq 'schema_violation' })
$good = $replies['c-good']
$holes = $replies['c-holes']
$liar = $replies['c-liar']
$list = $replies['c-list']
$listed = @($list.payload.data.instances | Where-Object { $_.id -eq 'echo:holes' })
$ext = $replies['c-ext']
$extListed = @($list.payload.data.instances | Where-Object { $_.id -eq 'echo:extended' })
$future = $replies['c-future']
$ancient = $replies['c-ancient']

Write-Host '== 判据 ==' -ForegroundColor Cyan
Check '1. **注册闸门响了**：家门少报一项的实例被点名' {
    $nonconforming.Count -ge 1 -and ($nonconforming.payload.instance -contains 'echo:holes')
}
Check '2. 说清**少的是哪一项**（不是一句含糊的"不合规"）' {
    $e = $nonconforming | Where-Object { $_.payload.instance -eq 'echo:holes' } | Select-Object -First 1
    ($e.payload.problems -join ' ') -like '*input_schema*'
}
Check '3. 合规的服务**没有被误报**（echo:1 / echo:liar / editor:1 都不在名单里）' {
    $named = @($nonconforming | ForEach-Object { $_.payload.instance })
    ($named -notcontains 'echo:1') -and ($named -notcontains 'echo:liar') -and ($named -notcontains 'editor:1')
}
Check '4. 它进了 degraded，而且 `list` 里看得见原因' {
    $listed.Count -eq 1 -and $listed[0].state -eq 'degraded' -and $listed[0].contract_errors.Count -ge 1
}
Check '5. **有活派给它会被挡下**，且拒绝指向"服务侧的问题"' {
    $holes -and $holes.payload.effect -eq 'refused' -and $holes.payload.why -like '*不合规*' -and `
        $holes.payload.remedy -like '*服务侧*'
}
Check '6. 拒绝里带上了具体缺的字段（照着就能改）' {
    $holes.payload.why -like '*input_schema*'
}
Check '7. **投递闸门响了**：说谎的那个结果带着 schema_violation' {
    $liar -and $liar.payload.schema_violation -and `
        (($liar.payload.schema_violation.problems -join ' ') -like '*data*')
}
Check '8. **结果没有被丢掉** —— 它照常回来了（丢掉只会让调用方等到超时）' {
    $liar.payload.effect -eq 'confirmed'
}
Check '9. 同一条结果也进了日志，而且带着那段判词（不变量二：模型可见即已记录）' {
    $logged = $records | Where-Object { $_.type -eq 'result' -and $_.corr -eq 'c-liar' } | Select-Object -First 1
    $logged -and $logged.payload.schema_violation
}
Check '10. 控制台上另有一条审计事件（"这个服务在说谎"是一件事实）' {
    $violationEvents.Count -ge 1 -and
        (($violationEvents.payload.why -join ' ') -like '*data*')
}
Check '11. **守规矩的服务一切照旧**：它的结果没有 schema_violation' {
    $good -and $good.payload.effect -eq 'confirmed' -and -not $good.payload.schema_violation -and
        $good.payload.data.text -eq '正常的那个'
}
<#
  第 12-14 条是 **2026-09-20** 加的：**沙箱那五个面成了必报项**。

  为什么值得单独立一条：在做它之前，"这套系统现在到底挡得住什么"**只能靠读代码猜** ——
  而且"围栏"这个词在代码里指两件不同的事（**绑定的范围** vs **沙箱**）。
  终端的 `run` 拉起的命令看得见整台机器，可它报的 `fence` 让人以为它被围住了。

  这三条判据盯的正是那个：**说了要围、却没围栏**，闸门要在**注册**时拦下来。
#>
$sandboxLiar = @($nonconforming | Where-Object { $_.payload.instance -eq 'echo:sandbox-liar' })
Check '12. **"说了要围、却没围栏"被注册闸门拦下**（新规则）' {
    $sandboxLiar.Count -ge 1
}
Check '13. 判词点的是**那一面**（`filesystem`），不是一句含糊的"不合规"' {
    ($sandboxLiar[0].payload.problems -join ' ') -like '*filesystem*'
}
Check '14. 而守着规矩的那些照旧（`echo:1` 与 `editor:1` 都没被误报）' {
    $named = @($nonconforming | ForEach-Object { $_.payload.instance })
    ($named -notcontains 'echo:1') -and ($named -notcontains 'editor:1') -and ($named -notcontains 'echo:liar')
}

Write-Host ''
Write-Host '== 那道"家门不合规"的判词 ==' -ForegroundColor Cyan
if ($nonconforming.Count -gt 0) {
    Write-Host "  instance: $($nonconforming[0].payload.instance)"
    Write-Host "  why:      $($nonconforming[0].payload.why)"
}
Write-Host ''
Write-Host '== 那道"结果不合形"的判词 ==' -ForegroundColor Cyan
if ($liar -and $liar.payload.schema_violation) {
    $liar.payload.schema_violation.problems | ForEach-Object { Write-Host "  - $_" }
}
Write-Host ''
Write-Host '== 派活给"家门不合规"的那个实例，中控怎么回 ==' -ForegroundColor Cyan
if ($holes) {
    Write-Host "  effect: $($holes.payload.effect)   code: $($holes.payload.code)"
    Write-Host "  why:    $($holes.payload.why)"
    Write-Host "  remedy: $($holes.payload.remedy)"
}
Write-Host ''
Check '15. **报了一个没声明过的实例状态 ⇒ 中控要说**（`instance_state_undeclared`）' {
    # 这条规则本身是老规矩（§11.8 的"两个轴"：实例事件的 `state` 必须出自
    # manifest 的 `state_vocabulary`）—— 但**"中控发现了会说"这半截一直没有判据**。
    # 它抓过一次真的：终端声明 `exited`、却每次 `close` 都报 `closed`。
    # 图上那个点的颜色就靠这张表，所以"报了个词表外的词"不许静悄悄过去。
    $st = @($records | Where-Object { $_.payload.code -eq 'instance_state_undeclared' })
    $st.Count -ge 1 -and $st[0].payload.state -eq 'zombie'
}
Check '16. 判词里要**给出它声明过的那几个词**（照着就能改，而不是只说"你报错了"）' {
    $st = @($records | Where-Object { $_.payload.code -eq 'instance_state_undeclared' } | Select-Object -First 1)
    $st -and (@($st.payload.declared) -contains 'idle') -and ($st.payload.why -like '*zombie*')
}
Check '17. 同一个词**只吵一次**（不然每报一次状态就刷一条审计）' {
    @($records | Where-Object { $_.payload.code -eq 'instance_state_undeclared' }).Count -eq 1
}
# 18-19. **能力名必须是认识的**（2026-09-27）：令牌表是**闭合**的。
# 拼错一个字母、或者编一个（`super.power`），签下去的都是"**看起来多了一条权限、
# 其实什么也管不住**"的能力 —— 用户以为给了权限，工具那边永远拒，两边的话还对不上。
# 这条闸门有**三个入口**：服务自报的 manifest（这儿）、`grant` op、服务表。
Check '18. **编一个能力名 ⇒ 注册关拦下**，并列出合法的那几个' {
    $capLiar = @($records | Where-Object { $_.payload.code -eq 'contract_violation' -or $_.payload.problems } |
        Where-Object { ($_.payload.problems -join ' ') -like '*super.power*' })
    $capLiar.Count -ge 1 -and ($capLiar[0].payload.problems -join ' ') -like '*fs.write*'
}

# 19-23. **版本对不上：说清，而且不认识的版本不许硬解释**（2026-09-27）。
#
# 在这之前，三处版本闸门全是**等号**（`v == CONTRACT_VERSION`）——
# 而**等号就是"没有兼容处理"**：它把"加一个可选字段"这种本该系统性地兼容的改动，
# 变成了"所有老服务当场被拒"。规则现在只有一条：**看 major**（规范 §9 / §11.6）。
#
# 三个替身各演一档：`2.0` 太新、`0.9` 太老、`1.9` 同 major 但更新。
$futureBad = @($nonconforming | Where-Object { $_.payload.instance -eq 'echo:future' })
$ancientBad = @($nonconforming | Where-Object { $_.payload.instance -eq 'echo:ancient' })
$futureWhy = if ($futureBad.Count -gt 0) { $futureBad[0].payload.problems -join ' ' } else { '' }
$ancientWhy = if ($ancientBad.Count -gt 0) { $ancientBad[0].payload.problems -join ' ' } else { '' }
Check '19. **未来的 major ⇒ 按名字拒**，判词里两个版本号都在（"你是几 / 我会的是几"）' {
    $futureBad.Count -ge 1 -and $futureWhy -like '*2.0*' -and $futureWhy -like '*1.0*'
}
Check '20. 它**活着但接不了活**：派活被挡下（`degraded` 那条老规矩照旧）' {
    $future -and $future.payload.effect -eq 'refused'
}
Check '21. **老的那一档判词不一样** —— "比我老"和"比我新"要分得开，不能同一句搪塞' {
    $ancientBad.Count -ge 1 -and $ancientWhy -ne $futureWhy -and
        $ancientWhy -like '*老*' -and $futureWhy -notlike '*老*'
}
Check '22. **同 major 的更新 minor ⇒ 收下**（这条原来会被等号拒掉）' {
    $named = @($nonconforming | ForEach-Object { $_.payload.instance })
    ($named -notcontains 'echo:extended') -and
        @($list.payload.data.instances | Where-Object { $_.id -eq 'echo:extended' -and $_.state -ne 'degraded' }).Count -eq 1 -and
        $ext -and $ext.payload.effect -eq 'confirmed' -and $ext.payload.data.text -eq '带扩展的那个'
}
Check '23. 但它**要出声**：日志里有一条"它在用我还不认识的追加字段"' {
    $note = @($records | Where-Object { $_.payload.code -eq 'contract_extensions_ignored' })
    $note.Count -ge 1 -and (($note[0].payload.notes -join ' ') -like '*1.9*')
}
Check '24. 那条备注**在 `list` 上也看得见**（中控不解释它，但不许闷着）' {
    $extListed.Count -eq 1 -and @($extListed[0].contract_notes).Count -ge 1 -and
        (@($extListed[0].contract_notes) -join ' ') -like '*1.9*'
}

Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "工作目录留在：$work"
