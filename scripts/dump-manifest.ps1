# **把某个服务真实报出来的 manifest 打出来。**
#
#   pwsh -File scripts/dump-manifest.ps1                 # 默认打编辑器那份
#   pwsh -File scripts/dump-manifest.ps1 -Service term   # editor / term / echo
#
# 为什么要有它：规范 §11.1 那份示例是**手写**的，所以它会和实现漂（2026-09-19
# 就漂过：`parameters` vs `input_schema`、`summary` vs `description`）。
# 写一个新工具服务的人要照抄的形状，**应该能直接从活的服务里 dump 出来**，
# 而不是照抄一段可能过期的 JSON。
#
# 另外：这么跑出来的 manifest 还会**顺带过一遍闸门** ——
# 中控收到 presence 时会查它（§11.8）。上面那门是注册关，它不吼就说明合规。
param(
    [ValidateSet('editor', 'term', 'echo')][string]$Service = 'editor'
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$bin = Join-Path $root "target\debug\harness-$Service.exe"
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-dump-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $work -Force | Out-Null
$svcArgs = @('--instance', "$Service`:1", '--sid', 's-dump')
if ($Service -ne 'echo') { $svcArgs += @('--fence', $work) }
# 服务上线就把 manifest 写在第一条 presence 里；这里只读那一行，之后让它自己退出。
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $bin
$psi.Arguments = ($svcArgs | ForEach-Object { if ($_ -match '\s') { '"' + $_ + '"' } else { $_ } }) -join ' '
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
$proc = [System.Diagnostics.Process]::Start($psi)
$line = $proc.StandardOutput.ReadLine()
$proc.StandardInput.Close()
$proc.WaitForExit(4000) | Out-Null
if (-not $line) { Write-Host "$Service 没有报 manifest" -ForegroundColor Red; exit 1 }
$msg = $line | ConvertFrom-Json
Write-Host "== $Service 自报的 manifest（$($msg.from)）==" -ForegroundColor Cyan
$msg.payload.manifest | ConvertTo-Json -Depth 20
Write-Host ''
Write-Host '（这份是从活的服务里 dump 出来的 —— 照它写，不要照文档里手写的那份抄）' -ForegroundColor DarkGray
