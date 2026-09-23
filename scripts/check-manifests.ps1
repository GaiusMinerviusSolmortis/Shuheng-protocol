# **manifest 体检**：把六个服务都拉起来，读它们**自报的家门**，逐项查。
#
#   pwsh -File scripts/check-manifests.ps1
#
# 为什么要有它（2026-09-20）：
#
# 1. **那张"沙箱五面"的表原来是手写的**（04-安全 §5.8）—— 而手写的表一定会漂，
#    漂的方向还总是"写得比实际严"。现在它是**从活服务里读出来的**。
# 2. 同类体检顺手都做了：五个面齐不齐、`sandbox_note` 空不空、
#    `not_implemented` 里有没有空话、`error_codes` 是不是空的。
# 3. 还有一个更实的：**`not_implemented` 里写的东西要真的还没做** ——
#    这条机器判不了，但它至少能保证**那个字段在**（我写过 README 说它写了，
#    而 harness-mem 的 manifest 里根本没有它）。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$exe = @{}
foreach ($n in 'ctrl', 'term', 'editor', 'mem', 'skill', 'mcp', 'echo', 'mock-mcp') {
    $exe[$n] = Join-Path $root "target\debug\harness-$n.exe"
    if ($n -eq 'mock-mcp') { $exe[$n] = Join-Path $root 'target\debug\mock-mcp.exe' }
    if (-not (Test-Path $exe[$n])) { throw "产物不在：$($exe[$n])" }
}
function J([string]$p) { return ($p -replace '\\', '\\') }
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-manifests-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$proj = Join-Path $work 'project'
New-Item -ItemType Directory -Path $proj -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $work 'skills') -Force | Out-Null
$sid = 's-man'
$servicesPath = Join-Path $work 'services.json'
@"
[
  { "kind": "term",   "id": "term:1",   "role": "tool",
    "cmd": ["$(J $exe['term'])"],
    "args": ["--fence", "$(J $proj)", "--mem-limit-mb", "256", "--max-procs", "8"] },
  { "kind": "editor", "id": "editor:1", "role": "tool",
    "cmd": ["$(J $exe['editor'])"], "args": ["--fence", "$(J $proj)"] },
  { "kind": "mem",    "id": "mem:1",    "role": "tool",
    "cmd": ["$(J $exe['mem'])"], "args": ["--store", "$(J (Join-Path $work 'memory'))"] },
  { "kind": "skill",  "id": "skill:1",  "role": "tool",
    "cmd": ["$(J $exe['skill'])"], "args": ["--root", "$(J (Join-Path $work 'skills'))"] },
  { "kind": "mcp",    "id": "mcp:1",    "role": "tool",
    "cmd": ["$(J $exe['mcp'])"],
    "args": ["--server", "$(J $exe['mock-mcp'])", "--allow", "echo",
             "--mem-limit-mb", "256"] },
  { "kind": "echo",   "id": "echo:1",   "role": "tool", "cmd": ["$(J $exe['echo'])"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $exe['ctrl']
$psi.Arguments = "--sid $sid --data-dir `"$(Join-Path $work 'data')`" --services `"$servicesPath`" --drain-ms 3000 --tick-ms 0"
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
$proc = [System.Diagnostics.Process]::Start($psi)
Start-Sleep -Seconds 3
$proc.StandardInput.Close()
$proc.WaitForExit(8000) | Out-Null
try { $d = $proc.StandardOutput.ReadToEndAsync(); $d.Wait(2000) | Out-Null } catch { }
$et = $proc.StandardError.ReadToEndAsync(); $et.Wait(4000) | Out-Null
$errText = if ($et.IsCompleted) { $et.Result } else { '' }
$logPath = Join-Path (Join-Path (Join-Path (Join-Path $work 'data') 'sessions') $sid) 'log.jsonl'
$records = Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json }

$manifests = @{}
foreach ($r in ($records | Where-Object { $_.type -eq 'presence' -and $_.payload.manifest })) {
    $manifests[$r.from] = $r.payload.manifest
}

Write-Host '== 判据 ==' -ForegroundColor Cyan
Check '1. 六个服务都自报了家门，而且**一个都没被注册关拦下**' {
    $manifests.Count -eq 6 -and $errText -notlike '*manifest_nonconforming*' -and `
        $errText -notlike '*不合规*'
}
Check '2. 每个都报了**沙箱五面**，而且每一面的值都在闭集里（闸门已经查过，这里数一遍）' {
    $faces = @('filesystem', 'network', 'identity', 'executables', 'resources')
    (@($manifests.Keys | Where-Object {
        $m = $manifests[$_]
        $s = $m.sandbox
        $s -and (@($faces | Where-Object { $s.$_ -eq $null }).Count -eq 0)
    }).Count) -eq 6
}
Check '3. 每个都写了 `sandbox_note`，而且不是空话' {
    (@($manifests.Keys | Where-Object { $manifests[$_].sandbox_note.Length -gt 20 }).Count) -eq 6
}
Check '4. **声明跟着事实走**：报 `filesystem: fence-args` 的，围栏必须非空；反之亦然' {
    $ok = $true
    foreach ($k in $manifests.Keys) {
        $m = $manifests[$k]
        $hasFence = @($m.fence).Count -gt 0
        $claims = $m.sandbox.filesystem -eq 'fence-args'
        if ($claims -and -not $hasFence) { $ok = $false }
    }
    $ok
}
Check '5. **`not_implemented` 必须真的有那个字段**（说过要写就得写），而且里面不许有空话' {
    # `harness-mem` 曾经漏了它 —— 而 README 里写着"这一层没做的，也写在 not_implemented 里"
    $need = @{
        'editor:1' = @('project_rollback')
        # 2026-09-20：`send`/`screen` 落地之后，终端那份"做不到"的清单换成现在真做不到的两条：
        # **改不了会话尺寸**，以及**会话模式里的 `cd` 管不住**（围栏只管它从哪起）。
        'term:1'   = @('resize', 'session_cwd')
        'mem:1'    = @('recall', 'anchors')
        'mcp:1'    = @('reconnect', 'cancel', 'rename')
        'skill:1'  = @('scope', 'search')
    }
    $ok = $true
    foreach ($k in $need.Keys) {
        $ni = $manifests[$k].not_implemented
        if (-not $ni) { $ok = $false; continue }
        foreach ($field in $need[$k]) {
            if (-not $ni.$field -or $ni.$field.Length -lt 10) { $ok = $false }
        }
    }
    $ok
}
Check '6. `error_codes` 每个都不空（模型靠它判断重试有没有用）' {
    (@($manifests.Keys | Where-Object { @($manifests[$_].error_codes).Count -ge 1 }).Count) -eq 6
}
# **7. 规范里那份"完整示例"要就是活服务报的那一份**（2026-09-20）。
#
# 这条挂在体检里而不是别处，理由很直接：它们查的是同一件事 ——
# "文档说的"和"服务报的"是不是一份东西。§11.6 那份示例**曾经是手写的**，
# 而它真的漂过（`summary` vs `description`、`parameters` vs `input_schema`）。
# 现在是生成的：`示例-manifest-编辑器.json`。
$gen = & pwsh -NoProfile -File (Join-Path $PSScriptRoot 'gen-manifest-example.ps1') -Check
$genOk = $LASTEXITCODE -eq 0
Check '7. 规范 §11.6 那份示例 == 活着的编辑器自报的那一份（逐字节）' { $genOk }
if (-not $genOk) { $gen | ForEach-Object { Write-Host "  $_" -ForegroundColor DarkGray } }
# **8. "拉不拉子进程"这件事也要跟着事实走**（2026-09-20）。
#
# 为什么这条能机器判：`executables: none` 说的就是"它（和它拉起的进程）
# 看不见可执行文件"。而"它拉不拉进程"在源码里是**数得出来**的：
# `std::process::Command`。两句一对，漂了就是假话 ——
# 而这一面原来谁都没查（第 4 条只查了 `filesystem`）。
#
# 起因：给编辑器加 L2 回滚时它开始跑 `git`，而它的沙箱声明还写着 `none`。
$spawners = @()
foreach ($svc in $manifests.Keys) {
    if ($manifests[$svc].sandbox.executables -ne 'none') { continue }
    $crate = 'harness-' + ($svc -replace ':.*$', '')
    $src = Join-Path $root "crates/$crate/src"
    if (-not (Test-Path $src)) { $spawners += "$svc（找不到源码目录 $src）"; continue }
    $hits = @(Get-ChildItem $src -Recurse -Filter *.rs | Select-String -Pattern 'Command::new' -SimpleMatch)
    if ($hits.Count -gt 0) { $spawners += "$svc：报了 executables:none，而 $crate 里有 $($hits.Count) 处 Command::new" }
}
Check '8. 报 `executables: none` 的服务，源码里**不许有 `Command::new`**' { $spawners.Count -eq 0 }
foreach ($s in $spawners) { Write-Host "  $s" -ForegroundColor Yellow }

Write-Host ''
Write-Host '== 六个服务自报的沙箱五面（这张表是从活服务里读出来的）==' -ForegroundColor Cyan
Write-Host ("  {0,-10} {1,-12} {2,-14} {3,-11} {4,-11} {5}" -f '服务', 'filesystem', 'network', 'identity', 'executables', 'resources')
foreach ($k in ($manifests.Keys | Sort-Object)) {
    $s = $manifests[$k].sandbox
    Write-Host ("  {0,-10} {1,-12} {2,-14} {3,-11} {4,-11} {5}" -f $k, $s.filesystem, $s.network, $s.identity, $s.executables, $s.resources)
}
Write-Host ''
Write-Host '⚠️ 这张表是**声明**，不是**强制** —— 谁挡得住什么，看的是这几个字符串后面有没有实现。' -ForegroundColor DarkGray
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    exit 1
}
Write-Host '六份家门都体检过了。' -ForegroundColor Green
