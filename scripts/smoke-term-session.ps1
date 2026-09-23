# **终端会话模式**（02-工具 §5.18 / §5.19）（2026-09-20）。
#
#   pwsh -File scripts/smoke-term-session.ps1
#
# 命令模式是**每条命令新起一个 `cmd /C`**；会话模式是**一条常驻的 shell**（真伪控制台）。
# 这一份要证四件事：
#
#   1. **会话起得来**，而且 shell 在**托管的那个目录**里；
#   2. **能往里敲、能看屏幕**（`send` / `screen`）—— 这是"能跟交互式程序打交道"的全部含义；
#   3. **`read` 是流、`screen` 是网格**（§5.19 那条"终端不是打印机"）：
#      一段被 `cls` 擦掉的内容，**流里有、网格里没有**；
#   4. **两种模式分得开**：命令模式的实例上 `send`/`screen` 明确拒；
#      会话模式的实例上 `run` 也明确拒（别让它以为是同一条会话）；
#   5. **模式算实例身份的一部分**：同一个目录上，命令模式一个、会话模式一个，
#      而同一个模式的重复 `open` 是幂等的（不会悄悄多起一条 PTY）；
#   6. **会话是有状态的**（环境变量、cwd 是那条 shell 自己记着的）——
#      这一条才是"实例是一条活着的进程"和"实例只是一条路径"的分界，
#      所以这两条判据**在命令模式上必然失败**（那边每条命令都是新进程）。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$term = Join-Path $root 'target\debug\harness-term.exe'
foreach ($b in @($ctrl, $term)) { if (-not (Test-Path $b)) { throw "产物不在：$b" } }
function J([string]$p) { return ($p -replace '\\', '\\') }
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-sess-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null
# 一个**只有那个目录里才有**的记号：用来证明 shell 的 cwd 是对的
Set-Content -LiteralPath (Join-Path $project 'MARKER-HERE.txt') -Value 'x' -Encoding utf8
# 一个子目录：用来证"cd 之后下一条命令还在那儿"（cwd 是 shell 自己记着的）
New-Item -ItemType Directory -Path (Join-Path $project 'sub') -Force | Out-Null
$sid = 's-sess'
$dataDir = Join-Path $work 'data'
$servicesPath = Join-Path $work 'services.json'
@"
[
  { "kind": "term", "id": "term:1", "role": "tool",
    "cmd": ["$(J $term)"], "args": ["--fence", "$(J $project)"] }
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
    $m = [ordered]@{ id = "m-$script:seq"; ts = 0; type = 'request'; from = 'console'
                     to = $to; corr = $corr; sid = $script:sid; authority = 'user'; payload = $payload }
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
function Call([string]$to, [string]$corr, [hashtable]$payload) {
    Send $to $corr $payload
    return WaitCorr $corr
}
# **屏幕要等一会儿才长得出来**：敲进去的东西不是马上到
function ScreenUntil([string]$corr, [string]$needle, [int]$timeoutMs = 15000) {
    $deadline = [DateTime]::UtcNow.AddMilliseconds($timeoutMs)
    $last = $null
    while ([DateTime]::UtcNow -lt $deadline) {
        $last = (Call 'term:1' $corr @{ tool = 'screen'; args = @{ instance_id = 't-1' } }).payload
        if ($last.text -like "*$needle*") { return $last }
        Start-Sleep -Milliseconds 250
    }
    return $last
}
Start-Sleep -Seconds 2

Write-Host '== 开一条会话 ==' -ForegroundColor Cyan
$open = (Call 'term:1' 'o1' @{ tool = 'open'; args = @{ path = '.'; purpose = '会话模式验收'; session = $true; cols = 100; rows = 30 } }).payload
Check '1. `open {session: true}` 开出来的**是会话模式**（回执自己说）' {
    $open.effect -eq 'confirmed' -and $open.mode -eq 'session' -and $open.instance_id -eq 't-1'
}
Start-Sleep -Seconds 1
$screen1 = (Call 'term:1' 's1' @{ tool = 'screen'; args = @{ instance_id = 't-1' } }).payload
Check '2. 屏幕上有**真 shell 的横幅**（不是空网格）' {
    $screen1.effect -eq 'confirmed' -and $screen1.alive -eq $true -and $screen1.text -match 'Microsoft'
}
Check '3. 而且它**在托管的那个目录里**（不是 `C:\Windows`）' {
    # 这条是实测抓出来的 bug：围栏给的 `\\?\C:\…` 被 cmd 当成 UNC 拒了，
    # shell 于是落在 Windows 目录 —— 会话能起、能敲、能回话，**只是目录是错的**。
    $dir = (Call 'term:1' 'd1' @{ tool = 'send'; args = @{ instance_id = 't-1'; text = 'cd' } }).payload
    $s = (ScreenUntil 'd2' 'project')
    $s.text -like '*project*' -and $s.text -notlike '*UNC*'
}

Write-Host '== 往里敲 ==' -ForegroundColor Cyan
$null = Call 'term:1' 'x1' @{ tool = 'send'; args = @{ instance_id = 't-1'; text = 'echo SESSION-MARK' } }
$after = ScreenUntil 'x2' 'SESSION-MARK'
Check '4. 敲进去的东西**到了 shell**：屏幕上既有回显、也有它的输出（两次）' {
    ([regex]::Matches([string]$after.text, 'SESSION-MARK')).Count -ge 2
}
Check '5. 光标**在动**（不是死网格）：它是 0 起的行/列，而且过了几行' {
    $after.cursor.row -ge 1 -and $after.cursor.col -ge 0
}

Write-Host '== read 是流、screen 是网格（§5.19）==' -ForegroundColor Cyan
$null = Call 'term:1' 'x3' @{ tool = 'send'; args = @{ instance_id = 't-1'; text = 'echo WILL-BE-CLEARED' } }
$null = ScreenUntil 'x4' 'WILL-BE-CLEARED'
$null = Call 'term:1' 'x5' @{ tool = 'send'; args = @{ instance_id = 't-1'; text = 'cls' } }
Start-Sleep -Milliseconds 800
$scr = (Call 'term:1' 'x6' @{ tool = 'screen'; args = @{ instance_id = 't-1' } }).payload
$st = (Call 'term:1' 'x7' @{ tool = 'read'; args = @{ instance_id = 't-1' } }).payload
Check '6. **被 `cls` 擦掉的那行：流里还在**（read 是"发生过什么"）' {
    [string]$st.text -like '*WILL-BE-CLEARED*'
}
Check '7. **网格里没有了**（screen 是"现在长什么样"）—— 这就是为什么两个都要' {
    [string]$scr.text -notlike '*WILL-BE-CLEARED*'
}
Check '8. 没认出来的转义序列**如实报出来**（不装作全认识）' {
    $scr.PSObject.Properties.Name -contains 'unknown_sequences' -and $scr.unknown_sequences -ge 0
}

Write-Host '== 两种模式分得开 ==' -ForegroundColor Cyan
$o2 = (Call 'term:1' 'o2' @{ tool = 'open'; args = @{ path = '.'; purpose = '命令模式那个'; } }).payload
Check '9. 默认 `open` 还是**命令模式**（不破坏原来的行为）' {
    $o2.effect -eq 'confirmed' -and $o2.mode -eq 'command' -and $o2.instance_id -eq 't-2'
}
$bad1 = (Call 'term:1' 'b1' @{ tool = 'send'; args = @{ instance_id = 't-2'; text = 'dir' } }).payload
Check '10. 命令模式的实例上 `send`：拒，而且**说清怎么开一个会话实例**' {
    $bad1.effect -eq 'refused' -and $bad1.remedy -like '*session*'
}
$bad2 = (Call 'term:1' 'b2' @{ tool = 'screen'; args = @{ instance_id = 't-2' } }).payload
Check '11. 命令模式的实例上 `screen`：拒，并指出**命令模式该用 read**' {
    $bad2.effect -eq 'refused' -and $bad2.remedy -like '*read*'
}
$bad3 = (Call 'term:1' 'b3' @{ tool = 'run'; args = @{ instance_id = 't-1'; cmd = 'echo NOPE' } }).payload
Check '12. 会话模式的实例上 `run`：拒（别让它以为在同一条会话里跑）' {
    $bad3.effect -eq 'refused' -and $bad3.remedy -like '*send*'
}

Write-Host '== 再要一条会话 ==' -ForegroundColor Cyan
# **同一个目录、同一个持有者、同一个模式 ⇒ 幂等**：不许悄悄再起一条 PTY，
# 而且尺寸对不上（我们改不了尺寸）要**说出来**，不能装作按你要的开了。
$o3 = (Call 'term:1' 'o3' @{ tool = 'open'; args = @{ path = '.'; session = $true; cols = 200; rows = 30 } }).payload
Check '13. 同目录再要一条会话：**拿回原来那条**（幂等），且如实说尺寸改不了' {
    $o3.effect -eq 'confirmed' -and $o3.instance_id -eq 't-1' -and $o3.idempotent -eq $true `
        -and [string]$o3.note -like '*100x30*'
}

Write-Host '== 会话是有状态的（这就是它存在的理由）==' -ForegroundColor Cyan
# 第 14 条是**真分界线**：命令模式每条命令都是新起的进程 ⇒ 变量在那边必然展开不了
# （实测：`cmd /C "set X=1"` 之后再 `cmd /C "echo got-%X%"` 打出来的就是 `got-%X%`）。
# 会话模式过，是因为变量在**那条活着的 shell** 里。
# ⚠️ 第 15 条**不是**分界线：命令模式下这条也过 —— 因为那里的 `cd` 是**我们自己拦下来
# 改 `t.cwd`** 的（服务替 shell 记着）。它证的是"会话里 cwd 由 shell 自己记着"，
# 而命令模式那份"记得住"其实是**模拟**出来的，只认 `cd` 这一种写法。
$null = Call 'term:1' 'v1' @{ tool = 'send'; args = @{ instance_id = 't-1'; text = 'set LIFEPROOF=ABC42' } }
$null = Call 'term:1' 'v2' @{ tool = 'send'; args = @{ instance_id = 't-1'; text = 'echo got-%LIFEPROOF%' } }
$env1 = ScreenUntil 'v3' 'got-ABC42'
Check '14. 上一条命令设的**环境变量，下一条命令展开得出来**（同一个 shell 活着）' {
    # `got-ABC42` 只可能来自 shell 真的展开了那个变量（新起的进程会原样打出 `%LIFEPROOF%`）
    [string]$env1.text -like '*got-ABC42*'
}
$null = Call 'term:1' 'v4' @{ tool = 'send'; args = @{ instance_id = 't-1'; text = 'cd sub' } }
$null = Call 'term:1' 'v5' @{ tool = 'send'; args = @{ instance_id = 't-1'; text = 'cd' } }
$cwd2 = ScreenUntil 'v6' 'project\sub'
Check '15. `cd` 进子目录之后**下一条命令还在那儿**（cwd 攥在 shell 手里，不是我们记的）' {
    [string]$cwd2.text -match 'project\\sub'
}

Write-Host '== 会话也会结束 ==' -ForegroundColor Cyan
$null = Call 'term:1' 'e1' @{ tool = 'send'; args = @{ instance_id = 't-1'; text = 'exit' } }
$dead = (Call 'term:1' 'e2' @{ tool = 'screen'; args = @{ instance_id = 't-1' } }).payload
$deadline = [DateTime]::UtcNow.AddSeconds(8)
while ([DateTime]::UtcNow -lt $deadline -and $dead.alive -eq $true) {
    Start-Sleep -Milliseconds 300
    $dead = (Call 'term:1' 'e3' @{ tool = 'screen'; args = @{ instance_id = 't-1' } }).payload
}
Check '16. `exit` 之后屏幕说 **shell 退了**，而且给出退出码（§5.20 的"进程退出"那条触发）' {
    $dead.alive -eq $false -and $dead.exit_code -eq 0
}

$proc.StandardInput.Close()
$proc.WaitForExit(8000) | Out-Null
$errText = $proc.StandardError.ReadToEnd()
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    Write-Host '== 最后一次屏幕 ==' -ForegroundColor DarkGray
    if ($scr) { $scr.text -split "`n" | ForEach-Object { Write-Host ('  |' + $_ + '|') -ForegroundColor DarkGray } }
    Write-Host '== stderr ==' -ForegroundColor DarkGray
    $errText -split "`n" | ForEach-Object { if ($_.Trim()) { Write-Host ('  ' + $_) -ForegroundColor DarkGray } }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host ("工作目录留在：" + $work) -ForegroundColor DarkGray
