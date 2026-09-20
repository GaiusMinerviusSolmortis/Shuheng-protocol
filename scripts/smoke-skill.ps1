# **技能（知识）服务**的验收（01-内核 §3 的"技能"）。
#
#   pwsh -File scripts/smoke-skill.ps1
#
# 这一层要证的核心只有一句：**渐进披露真的能省上下文，而不是一句口号。**
# 所以判据围着三样转：
#
#   · `list` 给的是**名片**（名字 + 一句话 + 章节目录）—— 正文一个字都不许有；
#   · `open` 一章一章给，给不完回 `next_line`（**接着要，不是一次倒完**）；
#   · 找不到的时候要**把有哪些列出来**（报错即手册），别让模型猜名字。
#
# 另外两条也在这里钉住：**坏掉的技能目录要报出来**（少了三份和"那三份坏了"是两件事），
# 以及**刚写的技能不用重启服务就能看见**（技能是文件，写它的人不会来通知你）。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$skillBin = Join-Path $root 'target\debug\harness-skill.exe'
$agentBin = Join-Path $root 'target\debug\harness-agent.exe'
foreach ($b in @($ctrl, $skillBin, $agentBin)) { if (-not (Test-Path $b)) { throw "产物不在：$b" } }
function J([string]$p) { return ($p -replace '\\', '\\') }
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-skill-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$skills = Join-Path $work 'skills'
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null
# 技能一：**裸 `SKILL.md`**（现成的技能目录大多长这样，按 `## ` 自动切章）
$s1 = Join-Path $skills 'adb-record'
New-Item -ItemType Directory -Path $s1 -Force | Out-Null
$body = @'
一份讲怎么录安卓调试视频的技能。

## 快速开始

用 `adb shell screenrecord /sdcard/x.mp4`，然后 `adb pull` 回来。

记住 r1 这一行是用来验分页的。
'@
# 造一份够长的正文：120 行一页，所以要到第二页才看得见结尾
$pad = (1..140 | ForEach-Object { "第 $_ 行：填长度用。" }) -join "`n"
Set-Content -LiteralPath (Join-Path $s1 'SKILL.md') -Value ($body + "`n" + $pad + "`n结尾这一行在第二页。") -Encoding utf8
# 技能二：**带 skill.json**
$s2 = Join-Path $skills 'pdf-report'
New-Item -ItemType Directory -Path $s2 -Force | Out-Null
@'
{"name":"pdf-report","title":"出 PDF 报告","version":"3",
 "summary":"怎么把数据做成一份能交出去的 PDF",
 "when_to_use":"要交报告的时候",
 "chapters":[{"id":"deps","title":"先装依赖","file":"01.md"},
             {"id":"make","title":"再生成","file":"02.md"}]}
'@ | Set-Content -LiteralPath (Join-Path $s2 'skill.json') -Encoding utf8
Set-Content -LiteralPath (Join-Path $s2 '01.md') -Value '先装依赖：reportlab。' -Encoding utf8
Set-Content -LiteralPath (Join-Path $s2 '02.md') -Value '再生成：跑 build_report。' -Encoding utf8
# 技能三：**坏的**（空目录）—— 它必须被跳过并报出来
New-Item -ItemType Directory -Path (Join-Path $skills 'broken-one') -Force | Out-Null

$sid = 's-skill'
$dataDir = Join-Path $work 'data'
# 两个智能体，**差别只在凭证**（同 smoke-regrant 的 A/B 那个形状）：
#   dada —— 签了 `skill.read`，它调 list 该成功；
#   zero —— 绑了同一个实例、**一条能力都没签**，它调 list 该被**服务自己**挡下来。
$scriptDada = Join-Path $work 'script-dada.ndjson'
@'
{"step":"tool","calls":[{"to":"skill:1","tool":"list","args":{}}]}
{"step":"text","text":"我看了技能清单。"}
'@ | Set-Content -LiteralPath $scriptDada -Encoding utf8
$scriptZero = Join-Path $work 'script-zero.ndjson'
@'
{"step":"tool","calls":[{"to":"skill:1","tool":"list","args":{}}]}
{"step":"text","text":"我看不了。"}
'@ | Set-Content -LiteralPath $scriptZero -Encoding utf8
$servicesPath = Join-Path $work 'services.json'
@"
[
  { "kind": "skill", "id": "skill:1", "role": "tool",
    "cmd": ["$(J $skillBin)"], "args": ["--root", "$(J $skills)"] },
  { "kind": "agent", "id": "agent:dada", "role": "agent",
    "bindings": [{"instance": "skill:1", "capabilities": ["skill.read"]}],
    "cmd": ["$(J $agentBin)"],
    "args": ["--persona", "达达", "--fence", "$(J $project)", "--script", "$(J $scriptDada)"] },
  { "kind": "agent", "id": "agent:zero", "role": "agent",
    "bindings": [{"instance": "skill:1", "capabilities": []}],
    "cmd": ["$(J $agentBin)"],
    "args": ["--persona", "零能力", "--fence", "$(J $project)", "--script", "$(J $scriptZero)"] }
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
function Send([string]$to, [string]$corr, [hashtable]$payload, [string]$from = 'console') {
    $script:seq++
    $m = [ordered]@{ id = "m-$($script:seq)"; ts = 0; type = 'request'; from = $from
                     to = $to; corr = $corr; sid = $sid; authority = 'user'; payload = $payload }
    $proc.StandardInput.WriteLine(($m | ConvertTo-Json -Compress -Depth 12))
    $proc.StandardInput.Flush()
}
function WaitCorr([string]$corr, [int]$timeoutMs = 25000) {
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
# 等服务起来
for ($i = 0; $i -lt 20; $i++) {
    Start-Sleep -Milliseconds 150
    Send 'ctrl' "ready-$i" @{ op = 'list' }
    $r = WaitCorr "ready-$i" 800
    if ($r -and (@($r.payload.data.instances | ForEach-Object { $_.id }) -contains 'skill:1')) { break }
}

Write-Host '== 判据 ==' -ForegroundColor Cyan
Send 'skill:1' 'l1' @{ tool = 'list'; args = @{} }
$l1 = WaitCorr 'l1'
Send 'skill:1' 'o1' @{ tool = 'open'; args = @{ skill = 'adb-record' } }
$o1 = WaitCorr 'o1'
Send 'skill:1' 'o2' @{ tool = 'open'; args = @{ skill = 'adb-record'; chapter = 'ch-1' } }
$o2 = WaitCorr 'o2'
Send 'skill:1' 'o3' @{ tool = 'open'; args = @{ skill = 'adb-record'; chapter = 'ch-1'; from_line = $o2.payload.data.next_line } }
$o3 = WaitCorr 'o3'
Send 'skill:1' 'o4' @{ tool = 'open'; args = @{ skill = 'pdf-report'; chapter = 'deps' } }
$o4 = WaitCorr 'o4'
Send 'skill:1' 's1' @{ tool = 'search'; args = @{ query = 'screenrecord' } }
$s1 = WaitCorr 's1'
Send 'skill:1' 'e1' @{ tool = 'open'; args = @{ skill = '不存在的技能' } }
$e1 = WaitCorr 'e1'
Send 'skill:1' 'e2' @{ tool = 'open'; args = @{ skill = 'adb-record'; chapter = '没有这章' } }
$e2 = WaitCorr 'e2'
Send 'skill:1' 'st' @{ tool = 'status'; args = @{} }
$st = WaitCorr 'st'
# **刚写一份新技能，不重启服务** —— 下一条 list 就该看得见它
$s3 = Join-Path $skills 'late-arrival'
New-Item -ItemType Directory -Path $s3 -Force | Out-Null
Set-Content -LiteralPath (Join-Path $s3 'SKILL.md') -Value "后来才写的一份技能。`n`n## 用法`n`n就这么用。`n" -Encoding utf8
Send 'skill:1' 'l2' @{ tool = 'list'; args = @{} }
$l2 = WaitCorr 'l2'
# 同一件事，交给**两个只差凭证的智能体**各跑一遍
Send 'agent:dada' 'td' @{ task = '看一眼有哪些技能' }
$td = WaitCorr 'td'
Send 'agent:zero' 'tz' @{ task = '也看一眼有哪些技能' }
$tz = WaitCorr 'tz'

$proc.StandardInput.Close()
$proc.WaitForExit(8000) | Out-Null
try { $d = $proc.StandardOutput.ReadToEndAsync(); $d.Wait(2000) | Out-Null } catch { }
$et = $proc.StandardError.ReadToEndAsync(); $et.Wait(3000) | Out-Null
$errText = if ($et.IsCompleted) { $et.Result } else { '' }
$logPath = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl'
$records = Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json }

Check '1. 服务自报的 manifest **过了注册关**（坏掉的技能目录没把它拖下水）' {
    $l1.payload.effect -eq 'confirmed' -and $errText -notlike '*manifest 不合规*' -and `
        $errText -like '*1 份坏了被跳过*'
}
Check '2. `list` 给的是**名片**：名字 + 一句话 + 章节目录，**正文一个字都没有**' {
    $dump = $l1.payload.data.skills | ConvertTo-Json -Depth 8 -Compress
    $l1.payload.data.skills.Count -eq 2 -and $dump -like '*adb-record*' -and `
        $dump -notlike '*screenrecord*' -and $dump -notlike '*reportlab*'
}
Check '3. `open` 不带章给**章节目录**（还带每章行数）' {
    $c = $o1.payload.data.chapters | Where-Object { $_.id -eq 'ch-1' } | Select-Object -First 1
    $c -and $c.title -eq '快速开始' -and $c.lines -gt 100 -and $o1.payload.data.text -eq $null
}
Check '4. `open` 带章才给**正文**，而且**一份给不完会回 `next_line`**' {
    $o2.payload.data.text -like '*screenrecord*' -and $o2.payload.data.next_line -ne $null -and `
        $o2.payload.data.lines_total -gt 120
}
Check '5. 接着要下一段（`from_line`）：**结尾那行在第二页**，两页不重不漏' {
    $o3.payload.data.text -like '*结尾这一行在第二页*' -and $o3.payload.data.next_line -eq $null -and `
        $o3.payload.data.from_line -eq $o2.payload.data.next_line
}
Check '6. 带 `skill.json` 的技能也读得出来（`skill.json` 声明的那一章）' {
    $o4.payload.effect -eq 'confirmed' -and $o4.payload.data.text -like '*reportlab*'
}
Check '7. `search` 只给**命中那一行 + 行号**，不给全文' {
    $h = $s1.payload.data.hits | Select-Object -First 1
    $h -and $h.skill -eq 'adb-record' -and $h.chapter -eq 'ch-1' -and $h.line -ge 1 -and `
        $h.text -like '*screenrecord*' -and $s1.payload.data.hits.Count -eq 1
}
Check '8. 技能名写错：拒，并把**有哪些**列出来（不许让模型猜）' {
    $e1.payload.effect -eq 'refused' -and $e1.payload.remedy -like '*adb-record*' -and `
        $e1.payload.remedy -like '*pdf-report*'
}
Check '9. 章 id 写错：拒，并把**那几章**列出来' {
    $e2.payload.effect -eq 'refused' -and $e2.payload.remedy -like '*ch-1*'
}
Check '10. **坏掉的技能目录报出来了**（少了三份和"那三份坏了"是两件事）' {
    $sk = $st.payload.data.skipped | Where-Object { $_.dir -like '*broken-one*' } | Select-Object -First 1
    $sk -and $sk.why.Length -gt 0
}
Check '11. **刚写的技能不重启服务就看得见**（技能是文件，没人会来通知你）' {
    @($l2.payload.data.skills | ForEach-Object { $_.name }) -contains 'late-arrival'
}
Check '12. **能力校验在服务侧**：零能力的那个智能体被**服务自己**挡下来，并说清缺什么' {
    $refus = @($records | Where-Object { $_.from -eq 'skill:1' -and $_.to -eq 'agent:zero' -and `
        $_.payload.effect -eq 'refused' })
    $refus.Count -ge 1 -and $refus[0].payload.why -like '*skill.read*' -and `
        ($refus[0].payload.granted -like '*什么都没有*')
}
Check '13. 而签了 `skill.read` 的那个智能体**同一条调用是成功的**（差别只在凭证）' {
    @($records | Where-Object { $_.from -eq 'skill:1' -and $_.to -eq 'agent:dada' -and `
        $_.payload.effect -eq 'confirmed' }).Count -ge 1
}
Check '14. 这一路没有任何一条结果**违反自己声明的形**' {
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
Write-Host "技能根目录留在：$skills"
