# **"这是哪种错"**：报错即手册那条纪律的验收（2026-09-20）。
#
#   pwsh -File scripts/smoke-args.ps1
#
# 起因是一次审计：拿空参数去调服务，回的是
#
#     code: instance_gone
#     why : 没有实例 ``        ← 两个反引号里是空的
#
# 两处都不对：
#   · **码选错了** —— §11.5 那张表里 `invalid` = "参数不对 ⇒ 看手册改"，
#     `instance_gone` = "你指的那个实例没了 ⇒ 重新 open"。**模型是照着码决定下一步的**；
#   · **那句话没教人** —— 空的名字看起来像渲染坏了。
#
# 还有一处同族的：`没有名为 X 的工具` 里那份"对的名字"是**手写的**，
# 而它已经漂了（编辑器少写了 `help` 和 `verify`）——
# **模型照着"别猜了"这句话学，也学不全**。
#
# 所以这一份判据的写法是**从服务自报的 manifest 里派生**：
# 谁是必填、有几个工具，全读它自己的名片，而不是抄一份进脚本。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$bin = Join-Path $root 'target\debug'

$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-args-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$fence = Join-Path $work 'proj'
New-Item -ItemType Directory -Path $fence -Force | Out-Null
Set-Content -LiteralPath (Join-Path $fence 'a.txt') -Value 'hello' -Encoding utf8
$skills = Join-Path $work 'skills'
New-Item -ItemType Directory -Path (Join-Path $skills 'demo') -Force | Out-Null
Set-Content -LiteralPath (Join-Path $skills 'demo\SKILL.md') -Value "# 演示`n正文" -Encoding utf8

function Start-Svc([string]$exe, [string[]]$svcArgs) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = Join-Path $bin $exe
    $psi.Arguments = ($svcArgs | ForEach-Object { '"' + $_ + '"' }) -join ' '
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
    return [System.Diagnostics.Process]::Start($psi)
}
# 读一行（非阻塞、带超时）—— 服务上线先自报家门，那条就是 manifest
function ReadLine2($p, $pending, [int]$ms = 5000) {
    if ($null -eq $pending) { $pending = $p.StandardOutput.ReadLineAsync() }
    if (-not $pending.Wait($ms)) { return @($null, $pending) }
    return @($pending.Result, $null)
}
$script:seq = 0
function Call($p, $pending, [string]$tool, [string]$argsJson) {
    $script:seq++
    $corr = "c$($script:seq)"
    $m = '{"id":"m-' + $script:seq + '","ts":0,"type":"request","from":"ctrl","to":"x:1","corr":"' + $corr +
         '","sid":"s1","authority":"user","payload":{"tool":"' + $tool + '","args":' + $argsJson +
         ',"granted":["*"],"caller_kind":"user"}}'
    $p.StandardInput.WriteLine($m)
    $p.StandardInput.Flush()
    $deadline = [DateTime]::UtcNow.AddSeconds(8)
    while ([DateTime]::UtcNow -lt $deadline) {
        $r = ReadLine2 $p $pending
        $line = $r[0]; $pending = $r[1]
        if ($null -eq $line) { break }
        if ($line.Trim() -eq '') { continue }
        $msg = $line | ConvertFrom-Json
        if ($msg.type -eq 'result' -and $msg.corr -eq $corr) { return @($msg.payload, $pending) }
    }
    return @($null, $pending)
}

$svcs = @(
  @{ name = 'editor'; exe = 'harness-editor.exe'; args = @('--fence', $fence) }
  @{ name = 'term';   exe = 'harness-term.exe';   args = @('--fence', $fence) }
  @{ name = 'mem';    exe = 'harness-mem.exe';    args = @('--store', (Join-Path $work 'store')) }
  @{ name = 'skill';  exe = 'harness-skill.exe';  args = @('--root', $skills) }
)

$manifests = @{}
$procs = @{}
$pendings = @{}
foreach ($s in $svcs) {
    $p = Start-Svc $s.exe $s.args
    $procs[$s.name] = $p
    Start-Sleep -Milliseconds 300
    $r = ReadLine2 $p $null 6000
    $pendings[$s.name] = $r[1]
    $line = $r[0]
    if (-not $line) { throw "$($s.name) 没有报 manifest" }
    $manifests[$s.name] = ($line | ConvertFrom-Json).payload.manifest
}

Write-Host ''
Write-Host '== 未知工具名：判词里要教**对的名字**（从它自己的 manifest 里读）==' -ForegroundColor Cyan
# ⚠️ **判据不许写在循环里**：`check-docs.ps1` 是按脚本里 `Check` 的**调用点**数条数的
# （今天它与实跑条数逐条相等）。循环会让静态数少于实跑数 —— 第一版这里就是在循环里，
# 于是 README 写 9 条、实跑 15 条。所以下面一律**展开成一行一条**。
$nameEditor = (Call $procs['editor'] $pendings['editor'] '绝对不是工具名' '{}')[0]
$nameTerm   = (Call $procs['term']   $pendings['term']   '绝对不是工具名' '{}')[0]
$nameMem    = (Call $procs['mem']    $pendings['mem']    '绝对不是工具名' '{}')[0]
$nameSkill  = (Call $procs['skill']  $pendings['skill']  '绝对不是工具名' '{}')[0]
function Covered($pay, $manifest) {
    # 判词里要把**真有的名字一个不漏**地说出来（少一个，模型照着学就少知道一个）
    @(@($manifest.tools | ForEach-Object { $_.name }) | Where-Object { $pay.remedy -notlike "*$_*" }).Count -eq 0
}
Check 'editor：未知工具 ⇒ invalid，名字一个不漏（9 个）' {
    $nameEditor.code -eq 'invalid' -and (Covered $nameEditor $manifests['editor'])
}
Check 'term：未知工具 ⇒ invalid，名字一个不漏（7 个）' {
    $nameTerm.code -eq 'invalid' -and (Covered $nameTerm $manifests['term'])
}
Check 'mem：未知工具 ⇒ invalid，名字一个不漏（9 个）' {
    $nameMem.code -eq 'invalid' -and (Covered $nameMem $manifests['mem'])
}
Check 'skill：未知工具 ⇒ invalid，名字一个不漏（6 个）' {
    $nameSkill.code -eq 'invalid' -and (Covered $nameSkill $manifests['skill'])
}

Write-Host ''
Write-Host '== 「你没填」和「它没了」必须是两种错 ==' -ForegroundColor Cyan
# **必填 instance_id 的工具从 manifest 里挑**（不是手抄一份）：
# 它们被**撒出去看结果**这件事本身可以循环，但按上面那条规矩，
# 每一行 Check 都写成显式的 —— 所以先把结果收齐，再一行一条地断言。
$needEd = @($manifests['editor'].tools | Where-Object { @($_.input_schema.required) -contains 'instance_id' } | ForEach-Object { $_.name })
$needTm = @($manifests['term'].tools | Where-Object { @($_.input_schema.required) -contains 'instance_id' } | ForEach-Object { $_.name })
$blankEd = @(); $goneEd = @(); $blankTm = @(); $goneTm = @()
foreach ($t in $needEd) {
    $r = Call $procs['editor'] $pendings['editor'] $t '{}'; $pendings['editor'] = $r[1]
    $blankEd += [pscustomobject]@{ tool = $t; pay = $r[0] }
    $r2 = Call $procs['editor'] $pendings['editor'] $t '{"instance_id":"x-99"}'; $pendings['editor'] = $r2[1]
    $goneEd += [pscustomobject]@{ tool = $t; pay = $r2[0] }
}
foreach ($t in $needTm) {
    $r = Call $procs['term'] $pendings['term'] $t '{}'; $pendings['term'] = $r[1]
    $blankTm += [pscustomobject]@{ tool = $t; pay = $r[0] }
    $r2 = Call $procs['term'] $pendings['term'] $t '{"instance_id":"x-99"}'; $pendings['term'] = $r2[1]
    $goneTm += [pscustomobject]@{ tool = $t; pay = $r2[0] }
}
Check "editor：$($needEd.Count) 个必填 instance_id 的工具，**空参数 ⇒ invalid**（不是 instance_gone）" {
    @($blankEd | Where-Object { $_.pay.code -ne 'invalid' }).Count -eq 0
}
Check 'editor：空参数那条的补救要**指向 open**' {
    @($blankEd | Where-Object { $_.pay.remedy -notlike '*open*' }).Count -eq 0
}
Check 'editor：给了不存在的 id ⇒ instance_gone' {
    @($goneEd | Where-Object { $_.pay.code -ne 'instance_gone' }).Count -eq 0
}
Check 'editor：两种错的判词**没有空的名字**（`` `` 那种）' {
    # ⚠️ 别用 `-like`：**wildcard 里反引号是转义符**，`'*``*'` 其实在找一个反引号，
    # 于是每一句都"命中"（第一版就这么写错了 —— 判据自己也踩了这个坑）。
    @(@($blankEd + $goneEd) | Where-Object { $_.pay.why.Contains('``') }).Count -eq 0
}
Check "term：$($needTm.Count) 个必填 instance_id 的工具，**空参数 ⇒ invalid**（不是 instance_gone）" {
    @($blankTm | Where-Object { $_.pay.code -ne 'invalid' }).Count -eq 0
}
Check 'term：空参数那条的补救要**指向 open**' {
    @($blankTm | Where-Object { $_.pay.remedy -notlike '*open*' }).Count -eq 0
}
Check 'term：给了不存在的 id ⇒ instance_gone' {
    @($goneTm | Where-Object { $_.pay.code -ne 'instance_gone' }).Count -eq 0
}
Check 'term：两种错的判词**没有空的名字**（`` `` 那种）' {
    @(@($blankTm + $goneTm) | Where-Object { $_.pay.why.Contains('``') }).Count -eq 0
}

Write-Host ''
Write-Host '== 手册自己别漂：manifest 里那张章节表和 help 报的那张**是同一张** ==' -ForegroundColor Cyan
# 同一族第五次：**同一件事被手写在两处**（manifest 的 `help_sections` 一份、
# `help` 回复里再列一份）。今天四家都对得上 —— 这条判据是**保险**，不是补窟窿：
# 加了章节忘改另一处，模型就会看到"手册里有、取不到"或者反过来。
$helpEd = (Call $procs['editor'] $pendings['editor'] 'help' '{}')[0]
$helpTm = (Call $procs['term']   $pendings['term']   'help' '{}')[0]
$helpMem = (Call $procs['mem']   $pendings['mem']    'help' '{}')[0]
$helpSk = (Call $procs['skill']  $pendings['skill']  'help' '{}')[0]
function SameChapters($pay, $manifest) {
    $a = @($manifest.help_sections | ForEach-Object { $_.id }) -join ','
    $b = @($pay.data.sections | ForEach-Object { $_.id }) -join ','
    ($a -eq $b) -and ($a.Length -gt 0)
}
Check 'editor：manifest.help_sections == help 报的章节（顺序也要一样）' { SameChapters $helpEd $manifests['editor'] }
Check 'term：manifest.help_sections == help 报的章节' { SameChapters $helpTm $manifests['term'] }
Check 'mem：manifest.help_sections == help 报的章节' { SameChapters $helpMem $manifests['mem'] }
Check 'skill：manifest.help_sections == help 报的章节' { SameChapters $helpSk $manifests['skill'] }
# 手册里写着"要找内容就传 search" —— 那就得**真的能用**（说一套做一套也是漂）
$hitEd = (Call $procs['editor'] $pendings['editor'] 'help' '{"search":"锁"}')[0]
Check 'editor：help 的 search 真的能命中（"锁"至少命中一章）' {
    $hitEd.data.search -eq '锁' -and @($hitEd.data.sections).Count -ge 1
}

Write-Host ''
Write-Host '== 值和参数名分开说 / 抱怨的顺序 ==' -ForegroundColor Cyan
# ⚠️ 这几条**被改过一次**：第一版断言的是"`run {错 id}` 回 invalid"，
# 而修完顺序之后它回 `instance_gone` —— **那次是判据过期，不是代码坏了**。
# 顺序的规矩现在很明确：**先抱怨第一个缺的东西**（实例 → 工具自己的参数）。
$badPath = (Call $procs['editor'] $pendings['editor'] 'open' '{}')[0]
Check 'editor.open 没给 path：invalid，且补救教怎么填' {
    $badPath.code -eq 'invalid' -and $badPath.remedy.Length -gt 10 -and $badPath.why.Contains('path')
}

$noCmd = (Call $procs['term'] $pendings['term'] 'run' '{}')[0]
Check 'term.run 两个参数都没给：报的是**第一个**（instance_id），且指向 open' {
    $noCmd.code -eq 'invalid' -and $noCmd.remedy.Contains('open')
}
$badId = (Call $procs['term'] $pendings['term'] 'run' '{"instance_id":"x-99"}')[0]
Check 'term.run 给了不存在的 id、没给 cmd：先报**实例不在**（instance_gone）' {
    $badId.code -eq 'instance_gone'
}
# 开一个**真的**终端，再故意不给 cmd —— 这一条才走到"值"那一层
$opened = (Call $procs['term'] $pendings['term'] 'open' ('{"path":"' + $fence.Replace('\', '\\') + '"}'))[0]
# `instance_id` 在回包的**顶层**（不是 data 里面）—— 这条和编辑器一致
$realId = $opened.instance_id
$emptyCmd = (Call $procs['term'] $pendings['term'] 'run' ('{"instance_id":"' + $realId + '"}'))[0]
Check 'term.run 实例是对的、cmd 是空的：invalid 且**教怎么填**（一次一条）' {
    $emptyCmd.code -eq 'invalid' -and $emptyCmd.remedy.Contains('cmd') -and $emptyCmd.remedy.Contains('一条')
}

foreach ($s in $svcs) {
    $procs[$s.name].StandardInput.Close()
    if (-not $procs[$s.name].WaitForExit(2000)) { $procs[$s.name].Kill() }
}

Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
