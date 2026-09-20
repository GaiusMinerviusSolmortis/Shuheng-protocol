# **L2 回滚：git 的那一半**（02-工具 §5.24）（2026-09-20）。
#
#   pwsh -File scripts/smoke-gitcheckpoint.ps1
#
# 这是你那句"我倾向于在编辑器中自动带一个 git 操作方便回滚"落地的样子 ——
# 而规范给它的约束是硬的：
#
# > `git checkout` / `git reset` / `git stash` 会**覆盖用户在同一个仓库里自己未提交的改动**、
# > 污染用户的索引、抢 `index.lock`。§5.15 的写锁**挡不住用户的 IDE 和命令行**。
#
# 所以这条脚本的**头三条判据**全是"它没动你的东西"：
# 工作区、索引/状态、stash 列表 —— 建一个 checkpoint 前后必须**逐字节一样**。
# 那不是性能，那是这个东西能不能存在的前提。
#
# 而"恢复"是唯一会写工作区的一步，它守的是和 `revert` **同一条规矩**：
# 跨过用户的改动就拒。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$editor = Join-Path $root 'target\debug\harness-editor.exe'
if (-not (Test-Path $editor)) { throw "产物不在：$editor" }
# ⚠️ 别把这个函数叫 `Git` —— PowerShell 的函数名**不区分大小写**，
# 于是函数体里的 `& git` 会调回它自己（call depth overflow，第一版就是这么炸的）。
$gitExe = (Get-Command git -ErrorAction SilentlyContinue).Source
if (-not $gitExe) { throw '这台机器上没有 git —— 这条脚本需要它' }

$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
function RunGit([string]$dir, [string[]]$gitArgs) {
    # ⚠️ 参数**不能叫 `$args`** —— 那是 PowerShell 的自动变量，
    # `@args` 会展开成**空**，于是每条 git 命令都只打出用法文本，
    # 而"什么都没做"恰好让"前后一样"那些判据**全都假装通过**。
    # 第一版就是这么错的（判据 3/4 因此假绿，中控说"这不是 git 仓库"是真话）。
    $out = & $gitExe -C $dir @gitArgs 2>&1
    return ($out | Out-String).Trim()
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-gitcp-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$repo = Join-Path $work 'repo'
$plain = Join-Path $work 'not-a-repo'
$fresh = Join-Path $work 'fresh'
New-Item -ItemType Directory -Path $repo, $plain, $fresh -Force | Out-Null

# 一个**有第一个提交**的仓库（`stash create` 需要 HEAD 有东西可比）
RunGit $repo @('init', '-q') | Out-Null
Set-Content -LiteralPath (Join-Path $repo 'a.txt') -Value "第一版`n" -Encoding utf8 -NoNewline
RunGit $repo @('add', 'a.txt') | Out-Null
RunGit $repo @('-c', 'user.name=t', '-c', 'user.email=t@t', 'commit', '-q', '-m', '第一个提交') | Out-Null

# 一个**刚 init、还没有提交**的仓库 —— 那是单独一种错（不是"没有改动"）
RunGit $fresh @('init', '-q') | Out-Null
Set-Content -LiteralPath (Join-Path $fresh 'b.txt') -Value "还没提交过`n" -Encoding utf8 -NoNewline

function Start-Editor([string]$fence, [string]$sid) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $editor
    $psi.Arguments = "--instance editor:1 --sid $sid --fence `"$fence`""
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
    $p = [System.Diagnostics.Process]::Start($psi)
    $p.StandardOutput.ReadLine() | Out-Null   # 第一条 presence（manifest）
    return $p
}
function Call($p, [string]$tool, [string]$argsJson) {
    $script:seq++
    $corr = "c$($script:seq)"
    $m = '{"id":"m-' + $script:seq + '","ts":0,"type":"request","from":"ctrl","to":"editor:1","corr":"' + $corr +
         '","sid":"s-git","authority":"user","payload":{"tool":"' + $tool + '","args":' + $argsJson +
         ',"granted":["*"],"caller_kind":"user"}}'
    $p.StandardInput.WriteLine($m)
    $p.StandardInput.Flush()
    # ⚠️ **别在循环里每次新开一个 `ReadLineAsync`** —— 上一个还挂着的时候再开一个
    # 会撞"The stream is currently in use by a previous operation"，于是判据收到 null
    # 然后**红得莫名其妙**（这条脚本第一版就是那么错的）。读任务要**持着**。
    $key = $p.Id
    if (-not $script:pending.ContainsKey($key)) { $script:pending[$key] = $null }
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    while ([DateTime]::UtcNow -lt $deadline) {
        if ($null -eq $script:pending[$key]) { $script:pending[$key] = $p.StandardOutput.ReadLineAsync() }
        $t = $script:pending[$key]
        if (-not $t.Wait(8000)) { break }
        $line = $t.Result
        $script:pending[$key] = $null
        if ($null -eq $line) { break }
        if ($line.Trim() -eq '') { continue }
        $msg = $line | ConvertFrom-Json
        if ($msg.type -eq 'result' -and $msg.corr -eq $corr) { return $msg.payload }
    }
    return $null
}
function Stop-Editor($p) {
    $p.StandardInput.Close()
    if (-not $p.WaitForExit(3000)) { $p.Kill() }
}
$script:seq = 0
$script:pending = @{}

Write-Host '== 在真仓库里：存一个点 ==' -ForegroundColor Cyan
# **先证明前提**（2026-09-20 学的）：下面有几条判据是"前后一样"型的，
# 而"两次都失败"也是"一样" —— 那会让它们**假装通过**。
# 所以第 0 条先钉死"这个目录真的是仓库，而且有一个提交"。
Check '0. 前提：那个目录**真的是 git 仓库**，而且有一个提交' {
    (RunGit $repo @('rev-parse', '--is-inside-work-tree')) -eq 'true' -and `
        (RunGit $repo @('rev-parse', '--verify', 'HEAD')).Length -ge 7
}
$ed = Start-Editor $repo 's-git'
$opened = Call $ed 'open' '{"path":"a.txt"}'
$docId = $opened.instance_id
$edited = Call $ed 'edit' ('{"instance_id":"' + $docId + '","old":"第一版","new":"第二版"}')

# **建点之前先记下"你的活长什么样"** —— 三条判据全靠它
$fileBefore = Get-Content -LiteralPath (Join-Path $repo 'a.txt') -Raw
$statusBefore = RunGit $repo @('status', '--porcelain')
$stashBefore = RunGit $repo @('stash', 'list')

$cp = Call $ed 'checkpoint' '{"label":"第一次编辑之后"}'
$ref = $cp.checkpoint

Check '1. 建点成功，回的是**一个私有 ref**（`refs/agent/<会话>/<n>`）' {
    $cp.effect -eq 'confirmed' -and $ref -like 'refs/agent/s-git/*'
}
Check '2. **工作区一个字没动**' {
    (Get-Content -LiteralPath (Join-Path $repo 'a.txt') -Raw) -eq $fileBefore
}
Check '3. **索引与工作区状态一个字没动**（`git status --porcelain` 前后一样）' {
    (RunGit $repo @('status', '--porcelain')) -eq $statusBefore
}
Check '4. **没有进你的 stash 列表**（我们绕开了那条路）' {
    (RunGit $repo @('stash', 'list')) -eq $stashBefore
}
Check '5. 那个 ref 真的在仓库里（`rev-parse` 认得它）' {
    (RunGit $repo @('rev-parse', '--verify', $ref)).Length -ge 7
}
Check '6. `checkpoints` 列得出来（本会话的账）' {
    $list = Call $ed 'checkpoints' '{}'
    @($list.checkpoints | ForEach-Object { $_.checkpoint }) -contains $ref
}

Write-Host '== 恢复：退回那个点 ==' -ForegroundColor Cyan
$null = Call $ed 'edit' ('{"instance_id":"' + $docId + '","old":"第二版","new":"第三版"}')
$restored = Call $ed 'restore' ('{"instance_id":"' + $docId + '","checkpoint":"' + $ref + '"}')
Check '7. `restore` 把文件退回那个点时的内容（第三版 → 第二版）' {
    $restored.effect -eq 'confirmed' -and `
        ((Get-Content -LiteralPath (Join-Path $repo 'a.txt') -Raw) -eq $fileBefore)
}

Write-Host '== 用户在主仓库里动过的东西，谁都不许跨过去 ==' -ForegroundColor Cyan
# 模拟"用户在同一个仓库里自己改了"（我们的写锁挡不住这个 —— 那正是要防的）
Set-Content -LiteralPath (Join-Path $repo 'a.txt') -Value "用户自己改的第四版`n" -Encoding utf8 -NoNewline
$refused = Call $ed 'restore' ('{"instance_id":"' + $docId + '","checkpoint":"' + $ref + '"}')
Check '8. 跨过用户的改动 ⇒ **拒绝**，而且说清是哪一条规矩' {
    $refused.effect -eq 'refused' -and $refused.why -like '*用户*' -and $refused.remedy -like '*用户*'
}
Check '9. 拒绝了就**真的没写**（用户那份还在）' {
    (Get-Content -LiteralPath (Join-Path $repo 'a.txt') -Raw) -like '*用户自己改的第四版*'
}
Stop-Editor $ed

Write-Host '== 不是仓库 / 还没有第一个提交：各自说清是哪一种 ==' -ForegroundColor Cyan
$ed2 = Start-Editor $plain 's-plain'
$noRepo = Call $ed2 'checkpoint' '{}'
Stop-Editor $ed2
Check '10. 不在 git 仓库里：拒绝，而且**指出 L1 照常可用**（不是一句"失败了"）' {
    $noRepo.effect -eq 'refused' -and $noRepo.remedy -like '*revert*'
}

$ed3 = Start-Editor $fresh 's-fresh'
$noCommit = Call $ed3 'checkpoint' '{}'
Stop-Editor $ed3
Check '11. 还没有第一个提交：**单独那种错**（不是含糊的"git 失败了"）' {
    $noCommit.effect -eq 'refused' -and $noCommit.why -like '*第一个提交*' -and $noCommit.remedy -like '*commit*'
}

Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "仓库留在：$repo" -ForegroundColor DarkGray
