# **中控被硬杀之后，它拉起来的那些进程会不会变成孤儿**（2026-09-20）。
#
#   pwsh -File scripts/smoke-orphan.ps1
#
# 为什么这不是洁癖：一个**活着的编辑器**会把那条路径的**域锁**一直攥着
# （命名互斥体，进程退出才释放）。于是"孤儿进程"的实际代价是：
# **下一次会话在同一路径上起不来编辑器**，而用户看到的是"域锁被占"——
# 一个已经没人在用的锁。所以这条要一起验"锁有没有跟着放"。
#
# 机制上它靠两件事（都在，但要有人钉住）：**stdin 的 EOF**（父子私有那条通道）
# 和**别把句柄传下去**（`NoInheritStdio`）。中控一死，管道就断，
# 服务读到 EOF 就自己收摊 —— **不需要中控在临终前挨个打招呼**
# （它要是被硬杀，本来也没机会打招呼）。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$bin = Join-Path $root 'target\debug'
$ctrl = Join-Path $bin 'harness-ctrl.exe'
foreach ($b in @($ctrl, (Join-Path $bin 'harness-echo.exe'), (Join-Path $bin 'harness-editor.exe'), (Join-Path $bin 'harness-term.exe'))) {
    if (-not (Test-Path $b)) { throw "产物不在：$b" }
}
function J([string]$p) { return ($p -replace '\\', '\\') }
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-orphan-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$proj = Join-Path $work 'project'
New-Item -ItemType Directory -Path $proj -Force | Out-Null
$servicesPath = Join-Path $work 'services.json'
@"
[
  { "kind": "echo",   "id": "echo:1",   "role": "tool", "cmd": ["$(J (Join-Path $bin 'harness-echo.exe'))"] },
  { "kind": "editor", "id": "editor:1", "role": "tool", "cmd": ["$(J (Join-Path $bin 'harness-editor.exe'))"],
    "args": ["--fence", "$(J $proj)"] },
  { "kind": "term",   "id": "term:1",   "role": "tool", "cmd": ["$(J (Join-Path $bin 'harness-term.exe'))"],
    "args": ["--fence", "$(J $proj)"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8

# 只数**这一次**中控拉起来的那些。
#
# 认法是**父进程 id**，不是"命令行里带没带这个临时目录" ——
# 后者漏掉 `echo`（它不吃 `--fence`，命令行里一个路径都没有），
# 第一版就是那么数的，于是基线数出来是 2 而不是 3。
$ctrlPid = 0
function MineAlive() {
    @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue |
        Where-Object {
            $_.ParentProcessId -eq $script:ctrlPid -and
            $_.Name -in @('harness-echo.exe', 'harness-editor.exe', 'harness-term.exe')
        })
}

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ctrl
$psi.Arguments = "--sid s-orph --data-dir `"$work\data`" --services `"$servicesPath`" --drain-ms 3000 --tick-ms 0"
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
$proc = [System.Diagnostics.Process]::Start($psi)
$script:ctrlPid = $proc.Id
Start-Sleep -Seconds 2

Write-Host '== 先看活着的时候 ==' -ForegroundColor Cyan
$alive = @(MineAlive)
Check '1. 三个服务都在（基线：这条判据得先证明"有东西可成为孤儿"）' {
    $alive.Count -eq 3
}

Write-Host '== 硬杀中控：不给它收摊的机会 ==' -ForegroundColor Cyan
$proc.Kill()
$proc.WaitForExit(3000) | Out-Null
Start-Sleep -Seconds 3
$left = @(MineAlive)

Check '2. 中控死了之后，那三个 stdio 服务**一个都不剩**（不许变成孤儿）' {
    $left.Count -eq 0
}
if ($left.Count -gt 0) {
    Write-Host "      还活着的是：$($left | ForEach-Object { $_.Name + '#' + $_.ProcessId } | Join-String -Separator ' / ')" -ForegroundColor DarkGray
}

# **孤儿进程真正的代价**：它攥着域锁 ⇒ 同一路径上新的编辑器起不来。
# 这一条比第 2 条更贴要害 —— 就算进程还在，只要锁放了，用户也不会被卡住。
$psi2 = New-Object System.Diagnostics.ProcessStartInfo
$psi2.FileName = Join-Path $bin 'harness-editor.exe'
$psi2.Arguments = "--instance editor:9 --sid s-orph2 --fence `"$proj`""
$psi2.RedirectStandardInput = $true
$psi2.RedirectStandardOutput = $true
$psi2.RedirectStandardError = $true
$psi2.UseShellExecute = $false
$psi2.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
$ed = [System.Diagnostics.Process]::Start($psi2)
Start-Sleep -Milliseconds 900
$edAlive = -not $ed.HasExited
$ed.StandardInput.Close()
if (-not $ed.WaitForExit(2500)) { $ed.Kill() }
$edErr = $ed.StandardError.ReadToEnd()

Check '3. **域锁也跟着放了**：同一个路径上，新的编辑器立刻能起来' {
    $edAlive -and ($edErr -notlike '*域冲突*')
}
if ($edErr -like '*域冲突*') {
    Write-Host "      新的编辑器被挡住了：$($edErr.Trim())" -ForegroundColor DarkGray
}

# 收尾：清掉这次可能残留的（正常情况下一条都没有）
foreach ($x in (MineAlive)) { try { Stop-Process -Id $x.ProcessId -Force -ErrorAction SilentlyContinue } catch { } }

Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
