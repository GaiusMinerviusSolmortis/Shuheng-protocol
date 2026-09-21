# **域锁的形状**：一个服务把它托管的**每一条**围栏都占上（2026-09-20）。
#
#   pwsh -File scripts/smoke-domainlock.ps1
#
# 这一条是**审计抓到的**（不是设计时想到的）：
# `domain_lock::acquire()` 原来是"遍历围栏、拿到一个就返回" —— 于是
# **只占了 `roots()[0]`**。而 `--fence` 是可重复的、manifest 里也写着
# `owns.exclusive_per: "fence"`，所以下面第二种情形是真的会漏：
#
#   [A,B] 与 [B,C] 各起一个编辑器 ⇒ **两个都拿到锁** ⇒ 两个都在管 B
#   ⇒ 各自的进程内写锁表互不知情 ⇒ §5.15 那套写互斥在这个组合下等于没有。
#
# 为什么以前没抓到：**34 个脚本里没有一条用过两个 `--fence`**。
#
# 判据要能分辨三种结局，所以盯的不是"进程还活着"（撞锁的服务**不退出**，
# 它进 degraded 继续接 `ping`/`status`），而是它 stderr 里那句话：
#   `[editor] 域锁拿到：…`  /  `[editor] 域冲突：…`
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$editor = Join-Path $root 'target\debug\harness-editor.exe'
if (-not (Test-Path $editor)) { throw "产物不在：$editor" }

$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-domainlock-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
foreach ($d in 'A', 'B', 'C') { New-Item -ItemType Directory -Path (Join-Path $work $d) -Force | Out-Null }
function RootOf([string]$n) { return (Join-Path $work $n) }

# 起一个编辑器并**保持它活着**（这样它手里那把锁才在）。
$script:started = New-Object System.Collections.Generic.List[object]
function StartEditor([string[]]$roots) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $editor
    $psi.Arguments = (($roots | ForEach-Object { '--fence "' + $_ + '"' }) -join ' ')
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
    $p = [System.Diagnostics.Process]::Start($psi)
    $script:started.Add($p) | Out-Null
    Start-Sleep -Milliseconds 900
    return $p
}
# 读它的 stderr（非阻塞，读到匹配或者超时）
function StderrOf($p, [int]$ms = 4000) {
    $sb = New-Object System.Text.StringBuilder
    $deadline = [DateTime]::UtcNow.AddMilliseconds($ms)
    while ([DateTime]::UtcNow -lt $deadline) {
        $t = $p.StandardError.ReadLineAsync()
        if (-not $t.Wait(400)) { break }
        $line = $t.Result
        if ($null -eq $line) { break }
        $sb.AppendLine($line) | Out-Null
        if ($line -like '*域锁拿到*' -or $line -like '*域冲突*') { break }
    }
    return $sb.ToString()
}
function StopAll() {
    foreach ($p in $script:started) {
        try { $p.StandardInput.Close() } catch { }
        if (-not $p.WaitForExit(1500)) { try { $p.Kill() } catch { } }
    }
    $script:started.Clear()
}
function KillOne($p) {
    try { $p.StandardInput.Close() } catch { }
    if (-not $p.WaitForExit(1500)) { try { $p.Kill() } catch { } }
    # ⚠️ **强杀之后要等它真的没了**（2026-09-20 修，这条判据偶发红过）：
    # `Process.Kill()` 返回**不等于进程已经退干净** —— 它还在拆的时候，
    # 那把命名互斥体还攥在手里，于是下一个编辑器会撞上"域锁被占"，
    # 而那是**被测对象完全正确**、夹具没等够。
    $null = $p.WaitForExit(5000)
    $deadline = [DateTime]::UtcNow.AddSeconds(5)
    while ([DateTime]::UtcNow -lt $deadline) {
        if (-not (Get-Process -Id $p.Id -ErrorAction SilentlyContinue)) { break }
        Start-Sleep -Milliseconds 50
    }
    $script:started.Remove($p) | Out-Null
    Start-Sleep -Milliseconds 200
}
$ok = '*域锁拿到*'
$no = '*域冲突*'

Write-Host ''
Write-Host '== 情形一：同一个围栏 —— 旧行为，不许坏 ==' -ForegroundColor Cyan
$p1 = StartEditor @((RootOf 'A'))
$e1 = StderrOf $p1
$p2 = StartEditor @((RootOf 'A'))
$e2 = StderrOf $p2
Check '1. 第一个拿到锁' { $e1 -like $ok }
Check '2. 围栏完全相同 ⇒ 第二个被挡住' { $e2 -like $no }

Write-Host ''
Write-Host '== 情形二：第一个围栏不同、但都托管 B —— 这就是那个洞 ==' -ForegroundColor Cyan
StopAll
$p3 = StartEditor @((RootOf 'A'), (RootOf 'B'))
$e3 = StderrOf $p3
$p4 = StartEditor @((RootOf 'B'), (RootOf 'C'))
$e4 = StderrOf $p4
Check '3. [A,B] 拿到锁' { $e3 -like $ok }
Check '4. **[B,C] 必须被挡住**：B 已经有主了' { $e4 -like $no }
Check '5. 而且报错要说清**是哪一条**被占了（B），不是笼统地说"冲突"' {
    $e4 -like $no -and $e4 -like ('*' + (RootOf 'B') + '*')
}
Check '6. 对称：反过来也要挡住' {
    StopAll
    $p5 = StartEditor @((RootOf 'B'), (RootOf 'C'))
    $e5 = StderrOf $p5
    $p6 = StartEditor @((RootOf 'A'), (RootOf 'B'))
    $e6 = StderrOf $p6
    ($e5 -like $ok) -and ($e6 -like $no)
}

Write-Host ''
Write-Host '== 情形三：不相干的两个围栏 —— 不许被顺手锁死 ==' -ForegroundColor Cyan
StopAll
$p7 = StartEditor @((RootOf 'A'))
$e7 = StderrOf $p7
$p8 = StartEditor @((RootOf 'B'))
$e8 = StderrOf $p8
Check '7. [A] 与 [B] 都起得来' { ($e7 -like $ok) -and ($e8 -like $ok) }

Write-Host ''
Write-Host '== 情形四：同一个根写两遍 —— 不许自己跟自己撞 ==' -ForegroundColor Cyan
StopAll
$p9 = StartEditor @((RootOf 'A'), (RootOf 'A'))
$e9 = StderrOf $p9
Check '8. `--fence A --fence A` 不该报冲突' { $e9 -like $ok -and $e9 -notlike $no }

Write-Host ''
Write-Host '== 情形五：持有者没了，锁要跟着放（内核回收，不靠人扫）==' -ForegroundColor Cyan
StopAll
$p10 = StartEditor @((RootOf 'A'), (RootOf 'B'))
$e10 = StderrOf $p10
KillOne $p10
$p11 = StartEditor @((RootOf 'B'), (RootOf 'C'))
$e11 = StderrOf $p11
Check '9. 前一个死了（连 A、B 一起放）⇒ 后一个拿得到' {
    ($e10 -like $ok) -and ($e11 -like $ok)
}

Write-Host ''
Write-Host '== 情形六：被拒的那次不许留下半套锁 ==' -ForegroundColor Cyan
# 它先按名字排序依次取 —— 撞上时要把**已经占上的**还回去。
# ⚠️ 这条是**单向**判据：只有当"它要的空闲那条"恰好排在"被占的那条"前面时，
# 才真的踩到释放那条路（排序是 hash 定的，我们不去凑）。
# 踩不到的时候它退化成一个"不许坏"的检查 —— 那也是它该有的样子。
StopAll
$p12 = StartEditor @((RootOf 'A'), (RootOf 'B'))
$e12 = StderrOf $p12
$p13 = StartEditor @((RootOf 'B'), (RootOf 'C'))
$e13 = StderrOf $p13
$p14 = StartEditor @((RootOf 'C'))
$e14 = StderrOf $p14
Check '10. 第二个被拒之后，C 仍然能被别人拿到（没留残锁）' {
    ($e12 -like $ok) -and ($e13 -like $no) -and ($e14 -like $ok)
}

StopAll
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "工作目录留在：$work"
