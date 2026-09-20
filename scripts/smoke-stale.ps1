# **用户改文件不是异常**：实例跟着磁盘走，而不是把门锁死。
#
#   pwsh -File scripts/smoke-stale.ps1
#
# §5.15 那句话要说准：**写锁保护的是"别的 AI 别大批量覆盖这个文件"，不是"用户别碰"。**
# 用户是主人，他随时会在自己的 IDE / 命令行里改同一个文件。
# 所以正确的策略不是拒绝，而是**跟着磁盘走**：
#
#   1. 发现磁盘变了 ⇒ **把它拉进来**，作为一条 `external` 的 revision（历史连贯、账上有它）；
#   2. `edit` 照常做替换 —— 安全性来自**内容**（`old` 必须唯一命中），不来自时间戳：
#      用户改的是别处 ⇒ 两边**并存**；用户改的正是那一处 ⇒ `old` 命不中，模型会收到"先读一眼";
#   3. `revert` 是**故意整份覆盖**，没有内容判据可依 ⇒ 它守另一条：
#      **不许跨过用户的改动**。
#
# 这份脚本自己**当那个用户**：直接往文件里写，然后看这个服务怎么反应。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$editor = Join-Path $root 'target\debug\harness-editor.exe'
function J([string]$p) { return ($p -replace '\\', '\\') }
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-stale-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null
$target = Join-Path $project 'notes.txt'
Set-Content -LiteralPath $target -Value "第一行`n" -Encoding utf8
$sid = 's-stale'
$dataDir = Join-Path $work 'data'
$servicesPath = Join-Path $work 'services.json'
@"
[ { "kind": "editor", "id": "editor:1", "role": "tool",
    "cmd": ["$(J $editor)"], "args": ["--fence", "$(J $project)"] } ]
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
$replies = @{}
function Ask([string]$corr, $body, [string]$to = 'editor:1') {
    $m = [ordered]@{ id = "m-$corr"; ts = 0; type = 'request'; from = 'console'; to = $to
                     corr = $corr; sid = $sid; authority = 'user'; payload = $body }
    $proc.StandardInput.WriteLine(($m | ConvertTo-Json -Compress -Depth 12))
    $proc.StandardInput.Flush()
    $deadline = [DateTime]::UtcNow.AddSeconds(20)
    while ([DateTime]::UtcNow -lt $deadline) {
        if ($null -eq $script:outTask) { $script:outTask = $proc.StandardOutput.ReadLineAsync() }
        if (-not $script:outTask.Wait(500)) { continue }
        $line = $script:outTask.Result
        $script:outTask = $null
        if ($null -eq $line -or $line.Trim() -eq '') { continue }
        $msg = $line | ConvertFrom-Json
        if ($msg.type -eq 'result' -and $msg.corr -eq $corr) { $replies[$corr] = $msg; return $msg }
    }
    return $null
}

$open = Ask 'o' @{ tool = 'open'; args = @{ path = 'notes.txt' } }
$id = $open.payload.instance_id
$e1 = Ask 'e1' @{ tool = 'edit'; args = @{ instance_id = $id; old = '第一行'; new = '第一行' + "`n" + '我加的' } }
Write-Host '== 现在**用户**（他自己的编辑器 / 命令行）也去改同一个文件 ==' -ForegroundColor Cyan
Set-Content -LiteralPath $target -Value "第一行`n他加的`n" -Encoding utf8
Start-Sleep -Milliseconds 20   # 让 mtime 明确不同
# 场景一：AI 手里那段已经被用户改没了 ⇒ **这一写会命不中**（内容判据起作用）
#   注意顺序是刻意的：**让"edit"这一路来发现**外界改动，它才会带上那提示
$e2 = Ask 'e2' @{ tool = 'edit'; args = @{ instance_id = $id; old = '我加的'; new = '我改的' } }
$diskAfterRefuse = Get-Content -LiteralPath $target -Raw
# 场景二：用户改的是**别处** ⇒ 两边的改动应该**并存**
$e3 = Ask 'e3' @{ tool = 'edit'; args = @{ instance_id = $id; old = '第一行'; new = '第一行' + "`n" + '我后来加的' } }
# 场景三：用户**接着又改了一笔**（这次是追加）⇒ `read` 应该把它拉进来并说出来
Add-Content -LiteralPath $target -Value "用户又改了`n" -Encoding utf8
Start-Sleep -Milliseconds 20
$r = Ask 'r' @{ tool = 'read'; args = @{ instance_id = $id } }
$v = Ask 'v' @{ tool = 'verify'; args = @{ instance_id = $id; expect = @{ contains = '他加的' } } }
# 场景四：**回滚不许跨过用户的改动**（它是整份覆盖）
$rv = Ask 'rv' @{ tool = 'revert'; args = @{ instance_id = $id; revision = 0 } }
$proc.StandardInput.Close()
$proc.WaitForExit(8000) | Out-Null
$et = $proc.StandardError.ReadToEndAsync(); $et.Wait(4000) | Out-Null
$onDisk = Get-Content -LiteralPath $target -Raw

$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
Write-Host '== 判据 ==' -ForegroundColor Cyan
Check '1. 自己改自己的：照常成功（新规矩不许误伤正常路径）' {
    $e1.payload.effect -eq 'confirmed'
}
Check '2. **命不中时它提示外界刚改过**（不是让人猜为什么）' {
    $e2.payload.effect -eq 'refused' -and $e2.payload.code -eq 'invalid' -and `
        $e2.payload.remedy -like '*外界改过*'
}
Check '3. 那次拒绝**没有动磁盘**（用户那版原封不动）' {
    ($diskAfterRefuse -like '*他加的*') -and ($diskAfterRefuse -notlike '*我改的*')
}
Check '4. **用户改的是别处 ⇒ 两边并存**：`第一行` 那处照常能改' {
    $e3.payload.effect -eq 'confirmed'
}
Check '5. **`read` 会把后来的改动拉进来**（`disk_changed` + 说明它成了 revision）' {
    $r.payload.disk_changed -eq $true -and $r.payload.text -like '*用户又改了*' -and `
        $r.payload.why -like '*拉进来*'
}
Check '6. **三笔改动全在**：用户的、用户的第二次、我的 —— 谁也没被抹掉' {
    ($onDisk -like '*他加的*') -and ($onDisk -like '*用户又改了*') -and ($onDisk -like '*我后来加的*')
}
Check '7. `verify` 从**磁盘**读：它看得到"他加的"（独立验证那一半照常）' {
    $v.payload.effect -eq 'confirmed' -and $v.payload.data.status -eq 'satisfied'
}
Check '8. **`revert` 不许跨过用户的改动**（整份覆盖那条没有内容判据可依）' {
    $rv.payload.effect -eq 'refused' -and $rv.payload.why -like '*用户的改动*'
}
Check '9. 拒绝里给的出口是"先看用户改了什么"，不是"硬退回去"' {
    $rv.payload.remedy -like '*read*' -and $rv.payload.remedy -like '*别退到它之前*'
}

Write-Host ''
Write-Host '== 命不中那次的拒绝（原话）==' -ForegroundColor Cyan
if ($e2) { Write-Host "  why:    $($e2.payload.why)"; Write-Host "  remedy: $($e2.payload.remedy)" }
Write-Host ''
Write-Host '== 回滚被拦那次（原话）==' -ForegroundColor Cyan
if ($rv) { Write-Host "  why:    $($rv.payload.why)"; Write-Host "  remedy: $($rv.payload.remedy)" }
Write-Host ''
Write-Host '== 磁盘上最终是什么 ==' -ForegroundColor Cyan
$onDisk -split "`n" | Where-Object { $_ -ne '' } | ForEach-Object { Write-Host "  $_" }
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "工作目录留在：$work"
