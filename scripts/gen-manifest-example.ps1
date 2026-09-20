# **重新生成规范里那份"完整示例"**（2026-09-20）。
#
#   pwsh -File scripts/gen-manifest-example.ps1          # 写 示例-manifest-编辑器.json
#   pwsh -File scripts/gen-manifest-example.ps1 -Check   # 只比，不写（check-manifests 用它）
#
# 为什么要有它（这是 README「已知的文档债」里挂了一阵的那条，也是决策 #25 那条机制）：
# 规范 §11.6 原来贴着一份**手写**的 manifest 示例 —— 而它**真的漂了**
# （`summary` vs `description`、`parameters` vs `input_schema`、
#  `open` 少了三个字段…… 2026-09-19 被闸门当场抓出来）。
# 修法不是"再手写一遍"，而是**别抄**：
#
#   活着的服务报什么 → 那个文件就是什么 → 体检时逐字节比一遍。
#
# 一条纪律：**这份文件是产物，不是源码**。要改它，改服务，然后跑这个脚本。
param([switch]$Check, [string]$Path)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
if (-not $Path) { $Path = Join-Path $root '示例-manifest-编辑器.json' }

Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$bin = Join-Path $root 'target\debug\harness-editor.exe'
if (-not (Test-Path $bin)) { throw "产物不在：$bin" }

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-genex-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $work -Force | Out-Null
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $bin
$psi.Arguments = "--instance editor:1 --sid s-gen --fence `"$work`""
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
$proc = [System.Diagnostics.Process]::Start($psi)
$line = $proc.StandardOutput.ReadLine()
$proc.StandardInput.Close()
$proc.WaitForExit(4000) | Out-Null
if (-not $line) { throw 'editor 没有报 manifest' }
$manifest = ($line | ConvertFrom-Json).payload.manifest

# **把会变的那一样换成固定的**：托管路径。它是个临时目录，每跑一次都不一样 ——
# 而其余每一个字节都是服务自己报的（这才是这份文件的价值）。
#
# 做法是**在文本上换**，不是"读进对象再序列化"：重序列化会把键序和空白重新排版，
# 于是"生成"和"活服务"之间又多出一层解释（而且 PowerShell 的对象不保证键序）。
$dump = $manifest | ConvertTo-Json -Depth 40
$raw = $manifest.fence[0]
if ($raw) {
    $escaped = $raw.Replace('\', '\\')
    $dump = $dump.Replace($escaped, 'D:\\proj')
}
$dump = ($dump -replace "`r`n", "`n").TrimEnd() + "`n"

# 还有一次自检：**换完之后不许剩下任何临时目录的痕迹**
$leftover = [regex]::Matches($dump, [regex]::Escape('harness-genex'))
if ($leftover.Count -gt 0) {
    throw "生成出来的示例里还留着临时目录（$($leftover.Count) 处）—— 换路径那一步没覆盖全"
}

if ($Check) {
    if (-not (Test-Path $Path)) {
        Write-Host "  [FAIL] $([System.IO.Path]::GetFileName($Path)) 不在（跑一次 gen-manifest-example.ps1 生成它）" -ForegroundColor Red
        exit 1
    }
    $onDisk = ((Get-Content $Path -Raw) -replace "`r`n", "`n").TrimEnd() + "`n"
    if ($onDisk -eq $dump) {
        Write-Host '  [pass] 规范里那份示例和活着的服务逐字节相同' -ForegroundColor Green
        exit 0
    }
    Write-Host '  [FAIL] 规范里那份示例和活服务**对不上**了' -ForegroundColor Red
    $a = $onDisk -split "`n"; $b = $dump -split "`n"
    $n = [Math]::Min($a.Count, $b.Count)
    $shown = 0
    for ($i = 0; $i -lt $n -and $shown -lt 8; $i++) {
        if ($a[$i] -ne $b[$i]) {
            Write-Host ("    第 " + ($i + 1) + " 行：") -ForegroundColor Yellow
            Write-Host ("      文件里： " + $a[$i]) -ForegroundColor DarkGray
            Write-Host ("      活服务： " + $b[$i]) -ForegroundColor DarkGray
            $shown++
        }
    }
    Write-Host '  重新生成：pwsh -File scripts/gen-manifest-example.ps1' -ForegroundColor Yellow
    exit 1
}

Set-Content -LiteralPath $Path -Value $dump -Encoding utf8 -NoNewline
Write-Host "写好了：$Path（$((Get-Content $Path).Count) 行）" -ForegroundColor Green
Write-Host '（它是**产物**：改了实现就重跑这个脚本，别手动编辑）' -ForegroundColor DarkGray
