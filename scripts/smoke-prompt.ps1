# **提示词实例**的验收（01-内核 §3 清单里的"提示词"）。
#
#   pwsh -File scripts/smoke-prompt.ps1
#
# §3 给"提示词"的定义是：**可被引用、可组合的文本片段，版本化，不需常驻**。
# 这一版把它落成 ctrl 的两个 op：
#
#   prompts           看库里有什么（名字 / 版本 / 字节数，**不给正文**）
#   prompt            把点名的几段**注入**某个智能体的上下文（**要 user 级别**）
#
# 为什么落在中控：片段的去处是**智能体的上下文**，而往上下文里塞东西的通道
# 只有 `inject` —— 方向规则一写着"只有中控能发"。所以"把哪一段念给谁听"
# 只能由中控做（§4.4）。
#
# 最要紧的一条判据是**前缀不重编**：注入是 append-only，
# 要是它顺手把前缀重写一遍，缓存就从那一刻起全废（§15.2）。
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

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-prompt-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
$prompts = Join-Path $work 'prompts'
New-Item -ItemType Directory -Path $project -Force | Out-Null
New-Item -ItemType Directory -Path $prompts -Force | Out-Null
# 两段提示词 + 一段**坏的**（空文件）—— "少了一段"和"那一段坏了"要分得开
@'
@version 1
写代码的规矩：
1. 改之前先读一遍那个文件。
2. 一次只做一件说得清的事。
'@ | Set-Content -LiteralPath (Join-Path $prompts 'coding-rules.md') -Encoding utf8
@'
审计模式：所有写操作都要在结论里点出来。
'@ | Set-Content -LiteralPath (Join-Path $prompts 'audit-mode.md') -Encoding utf8
Set-Content -LiteralPath (Join-Path $prompts 'empty-one.md') -Value '' -Encoding utf8

function Start-Ctrl2([string]$sid, [string]$tag, [bool]$withPrompts, [string]$channel, [string]$inject = '') {
    $dataDir = Join-Path $work "data-$tag"
    $scriptPath = Join-Path $work "script-$tag.ndjson"
    Set-Content -LiteralPath $scriptPath -Value '{"step":"text","text":"我在。"}' -Encoding utf8
    $servicesPath = Join-Path $work "services-$tag.json"
    # 注入策略那一档由**服务自己声明**（`--inject`），空的就用默认（`card`）
    $injectArgs = if ($inject) { '"--inject", "' + $inject + '"' } else { '' }
    @"
[
  { "kind": "echo", "id": "echo:1", "role": "tool", "cmd": ["$(J $echo)"], "args": [$injectArgs] },
  { "kind": "agent", "id": "agent:dada", "role": "agent", "bindings": ["echo:1"],
    "cmd": ["$(J $agentBin)"],
    "args": ["--persona", "达达", "--fence", "$(J $project)", "--script", "$(J $scriptPath)"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $ctrl
    $psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 5000 --tick-ms 0"
    if ($withPrompts) { $psi.Arguments += " --prompts `"$prompts`"" }
    if ($channel) { $psi.Arguments += " --channel $channel" }
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
    $proc = [System.Diagnostics.Process]::Start($psi)
    return [pscustomobject]@{ proc = $proc; sid = $sid; dataDir = $dataDir; outTask = $null }
}
function NextLineOf($h, [int]$timeoutMs) {
    if ($null -eq $h.outTask) { $h.outTask = $h.proc.StandardOutput.ReadLineAsync() }
    if (-not $h.outTask.Wait($timeoutMs)) { return $null }
    $line = $h.outTask.Result
    $h.outTask = $null
    return $line
}
$script:seq = 0
function Send($h, [string]$to, [string]$corr, [hashtable]$payload, [string]$from = 'console') {
    $script:seq++
    $m = [ordered]@{ id = "m-$($script:seq)"; ts = 0; type = 'request'; from = $from
                     to = $to; corr = $corr; sid = $h.sid; authority = 'user'; payload = $payload }
    $h.proc.StandardInput.WriteLine(($m | ConvertTo-Json -Compress -Depth 12))
    $h.proc.StandardInput.Flush()
}
function WaitCorr($h, [string]$corr, [int]$timeoutMs = 25000) {
    $deadline = [DateTime]::UtcNow.AddMilliseconds($timeoutMs)
    while ([DateTime]::UtcNow -lt $deadline) {
        $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
        $line = NextLineOf $h $left
        if ($null -eq $line) { continue }
        if ($line.Trim() -eq '') { continue }
        $m = $line | ConvertFrom-Json
        if ($m.type -eq 'result' -and $m.corr -eq $corr) { return $m }
    }
    return $null
}
function Wait-Agent($h, [int]$tries = 30) {
    for ($i = 0; $i -lt $tries; $i++) {
        Start-Sleep -Milliseconds 150
        Send $h 'ctrl' "ready-$($h.sid)-$i" @{ op = 'list' }
        $r = WaitCorr $h "ready-$($h.sid)-$i" 900
        if ($r -and (@($r.payload.data.instances | ForEach-Object { $_.id }) -contains 'agent:dada')) { return $r }
    }
    return $null
}
function Stop-Ctrl($h) {
    $h.proc.StandardInput.Close()
    $h.proc.WaitForExit(10000) | Out-Null
    try { $d = $h.proc.StandardOutput.ReadToEndAsync(); $d.Wait(2000) | Out-Null } catch { }
    $e = $h.proc.StandardError.ReadToEndAsync(); $e.Wait(4000) | Out-Null
    if ($e.IsCompleted) { return $e.Result } else { return '' }
}

Write-Host '== 场景 A：挂着提示词库，看 + 念 ==' -ForegroundColor Cyan
$A = Start-Ctrl2 's-pa' 'a' $true ''
$null = Wait-Agent $A
Send $A 'ctrl' 'l1' @{ op = 'prompts' }
$l1 = WaitCorr $A 'l1'
Send $A 'ctrl' 'p1' @{ op = 'prompt'; agent = 'agent:dada'; fragments = @('coding-rules', 'audit-mode') }
$p1 = WaitCorr $A 'p1'
# 让智能体跑一轮（敕令在那之前进上下文）
Send $A 'agent:dada' 't1' @{ task = '随便说一句' }
$t1 = WaitCorr $A 't1'
Send $A 'ctrl' 'e1' @{ op = 'prompt'; agent = 'agent:dada'; fragments = @('没有这一段') }
$e1 = WaitCorr $A 'e1'
Send $A 'ctrl' 'e2' @{ op = 'prompt'; agent = 'agent:小明'; fragments = @('audit-mode') }
$e2 = WaitCorr $A 'e2'
$Aerr = Stop-Ctrl $A
$Arecs = Get-Content -LiteralPath (Join-Path (Join-Path (Join-Path $A.dataDir 'sessions') $A.sid) 'log.jsonl') |
    Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json }
$Aindex = Get-ChildItem -Recurse -Filter instances.jsonl $A.dataDir |
    ForEach-Object { Get-Content $_.FullName } | Where-Object { $_.Trim() -ne '' } |
    ForEach-Object { $_ | ConvertFrom-Json }

Check '1. `prompts` 列出库里有什么：名字 / 版本 / 字节数' {
    $f = $l1.payload.data.fragments | Where-Object { $_.name -eq 'coding-rules' } | Select-Object -First 1
    $f -and $f.version -eq '1' -and $f.bytes -gt 10
}
Check '2. 但**看得到不等于念得到**：`prompts` 的返回里没有正文' {
    $dump = $l1.payload.data | ConvertTo-Json -Depth 6 -Compress
    $dump -notlike '*改之前先读一遍*'
}
Check '3. **坏的那一段报出来了**（少了一段和"那一段坏了"是两件事）' {
    $sk = $l1.payload.data.skipped | Where-Object { $_.name -eq 'empty-one' } | Select-Object -First 1
    $sk -and $sk.why.Length -gt 0
}
Check '4. 注入成功，回执里说清念了哪几段、各自哪一版' {
    $names = @($p1.payload.data.fragments | ForEach-Object { "$($_.name)@$($_.version)" })
    $p1.payload.effect -eq 'confirmed' -and ($names -contains 'coding-rules@1') -and ($names -contains 'audit-mode@0')
}
Check '5. 它真的**进了模型的上下文**：一条 injected，两段都带抬头、按顺序' {
    $inj = $Arecs | Where-Object { $_.type -eq 'inject' -and $_.payload.reason -eq 'prompt_fragment' } | Select-Object -First 1
    $idx = $Arecs | Where-Object { $_.payload.kind -eq 'model_item' -and `
        $_.payload.item.kind -eq 'injected' -and $_.payload.item.text -like '*coding-rules@1*' } | Select-Object -First 1
    $inj -and $idx -and `
        $idx.payload.item.text -like '*audit-mode@0*' -and `
        ($idx.payload.item.text.IndexOf('coding-rules@1') -lt $idx.payload.item.text.IndexOf('audit-mode@0'))
}
Check '6. **前缀没有重编**：`prompt_compiled` 全程只有一条（§15.2 那条铁律）' {
    @($Arecs | Where-Object { $_.payload.kind -eq 'prompt_compiled' }).Count -eq 1
}
Check '7. 名字写错：拒，并**列出库里有哪些**（不许让模型猜）' {
    $e1.payload.effect -eq 'refused' -and $e1.payload.remedy -like '*coding-rules*'
}
Check '8. 念给一个不存在的智能体：拒，并列出**现在有哪些**' {
    $e2.payload.effect -eq 'refused' -and $e2.payload.remedy -like '*agent:dada*'
}
Check '9. 注入这件事**进了身份索引**（一次控制面变更要留痕）' {
    @($Aindex | Where-Object { $_.event -eq 'prompt_injected' -and $_.id -eq 'agent:dada' }).Count -ge 1
}

Write-Host '== 场景 B：改文件之后不重启，念的是新版本 ==' -ForegroundColor Cyan
$B = Start-Ctrl2 's-pb' 'b' $true ''
$null = Wait-Agent $B
Send $B 'ctrl' 'v1' @{ op = 'prompt'; agent = 'agent:dada'; fragments = @('coding-rules') }
$v1 = WaitCorr $B 'v1'
# **不重启**，直接把那段改到第 2 版
@'
@version 2
写代码的规矩（第二版）：改之前先读，改完要说清改了什么。
'@ | Set-Content -LiteralPath (Join-Path $prompts 'coding-rules.md') -Encoding utf8
Send $B 'ctrl' 'v2' @{ op = 'prompt'; agent = 'agent:dada'; fragments = @('coding-rules') }
$v2 = WaitCorr $B 'v2'
Send $B 'ctrl' 'l2' @{ op = 'prompts' }
$l2 = WaitCorr $B 'l2'
$Berr = Stop-Ctrl $B

Check '10. **改文件不用重启**：第二次念的是 @2（提示词是文件，没人会来通知你）' {
    $n1 = @($v1.payload.data.fragments | ForEach-Object { "$($_.name)@$($_.version)" })
    $n2 = @($v2.payload.data.fragments | ForEach-Object { "$($_.name)@$($_.version)" })
    ($n1 -contains 'coding-rules@1') -and ($n2 -contains 'coding-rules@2')
}
Check '11. 而且 `prompts` 和 `prompt` **报的是同一份**（两处一致，别自相矛盾）' {
    $f = $l2.payload.data.fragments | Where-Object { $_.name -eq 'coding-rules' } | Select-Object -First 1
    $f -and $f.version -eq '2'
}

Write-Host '== 场景 C：没挂库 / 不是 user，都不许注入 ==' -ForegroundColor Cyan
$C = Start-Ctrl2 's-pc' 'c' $false ''
$null = Wait-Agent $C
Send $C 'ctrl' 'n1' @{ op = 'prompt'; agent = 'agent:dada'; fragments = @('coding-rules') }
$n1 = WaitCorr $C 'n1'
$Cerr = Stop-Ctrl $C
Check '12. 没挂提示词库时：拒，并说清**怎么挂**' {
    $n1.payload.effect -eq 'refused' -and $n1.payload.remedy -like '*--prompts*'
}

$D = Start-Ctrl2 's-pd' 'd' $true '微信:agent'
$null = Wait-Agent $D
# 这条通道的权威上限是 agent ⇒ 它发不出 user 级别的动作
Send $D 'ctrl' 'n2' @{ op = 'prompt'; agent = 'agent:dada'; fragments = @('audit-mode') } '微信'
$n2 = WaitCorr $D 'n2'
$Derr = Stop-Ctrl $D
Check '13. **只有用户能注入规矩**：一条被降级到 agent 的通道发不出来' {
    $n2.payload.effect -eq 'refused' -and $n2.payload.why -like '*user*'
}

Write-Host '== 场景 E：`injection_policy` **真的有执行者** ==' -ForegroundColor Cyan
# 这个字段原来是个**死声明**：五个服务都写 `card`，而没有任何一行代码读它
# （2026-09-20 审计）。按本仓库自己那条规矩（"声明一个没人执行的能力比不声明更糟"），
# 现在 `card` 与 `help` 由**提示词编译器**执行，`full` 由**注册关**挡下（它没有执行者）。
$E = Start-Ctrl2 's-pe' 'e' $false '' 'help'
$null = Wait-Agent $E
Send $E 'agent:dada' 'pe1' @{ task = '随便说一句' }
$pe1 = WaitCorr $E 'pe1'
$Eerr = Stop-Ctrl $E
$Erecs = Get-Content -LiteralPath (Join-Path (Join-Path (Join-Path $E.dataDir 'sessions') $E.sid) 'log.jsonl') |
    Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json }
$promptA = ($Arecs | Where-Object { $_.payload.kind -eq 'prompt_compiled' } | Select-Object -First 1).payload.system
$promptE = ($Erecs | Where-Object { $_.payload.kind -eq 'prompt_compiled' } | Select-Object -First 1).payload.system

Check '14. `card`（默认那档）：前缀里**没有**章节目录' {
    $promptA -and ($promptA -notlike '*手册有这些章*')
}
Check '15. `help` 那档：前缀里**有章节目录**（标题，不是正文）' {
    $promptE -and ($promptE -like '*手册有这些章*') -and ($promptE -like '*错误码与补救*')
}
Check '16. **只有标题进了前缀**：正文一个字都没进去' {
    # ⚠️ 得挑**正文里**的话，别挑名片里本来就有的话（第一版我拿了 `summary`
    # 里那句"只回话、不做事"，而那句话**本来就该在名片里** —— 判据错了，不是代码错）。
    $promptE -notlike '*把系统的 bug*' -and $promptE -notlike '*不碰文件、不跑命令*'
}

$F = Start-Ctrl2 's-pf' 'f' $false '' 'full'
$null = Wait-Agent $F
$Ferr = Stop-Ctrl $F
$Frecs = Get-Content -LiteralPath (Join-Path (Join-Path (Join-Path $F.dataDir 'sessions') $F.sid) 'log.jsonl') |
    Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json }
Check '17. `full`：**注册关把它拦下来**，理由点名"没有执行者"' {
    $bad = $Frecs | Where-Object { $_.payload.code -eq 'manifest_nonconforming' } | Select-Object -First 1
    $bad -and ($bad.payload.why -like '*没有执行者*')
}

Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    Write-Host '== A stderr ==' -ForegroundColor DarkGray
    foreach ($l in ($Aerr -split "`n")) { if ($l.Trim() -ne '') { Write-Host "  $l" -ForegroundColor DarkGray } }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "提示词库留在：$prompts"
