# **分页读**：编辑器那半（终端那半在 smoke-spill 里）。
#
#   pwsh -File scripts/smoke-pager.ps1
#
# §5.17 那条死循环（"读全文还得再跑一条命令"）有两个出口，两个服务各一个：
#
#   * **终端**：输出只活在内存/溢写文件里 ⇒ 内存留尾巴、前面溢到磁盘，`read` 跨两段按游标读；
#   * **编辑器**：全文**本来就是磁盘上那个文件** ⇒ 它不需要溢写副本，
#     只需要**分页**：一次给一页（内联上限），`next_offset` 指给你接着读。
#
# 所以两边 `read` 的形状是一样的（都是分页器），差别只在 `spilled_to`：
# 终端给"溢写文件在哪"，编辑器**给 null 并说清全文就在你打开的那个文件里**。
#
# 最要紧的一条判据：**分页读回来的字节，和磁盘上那个文件一字不差。**
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$editor = Join-Path $root 'target\debug\harness-editor.exe'
function J([string]$p) { return ($p -replace '\\', '\\') }
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-pager-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null
# 造一个一万多字节的文件（内联上限调到 1000 ⇒ 至少十几页）
$big = @()
for ($i = 1; $i -le 400; $i++) { $big += ("LINE-{0:D4}-XXXXXXXXXXXXXXXXXX" -f $i) }
$bigPath = Join-Path $project 'big.txt'
Set-Content -LiteralPath $bigPath -Value $big -Encoding utf8
$original = Get-Content -LiteralPath $bigPath -Raw
$sid = 's-pager'
$dataDir = Join-Path $work 'data'
$servicesPath = Join-Path $work 'services.json'
@"
[ { "kind": "editor", "id": "editor:1", "role": "tool",
    "cmd": ["$(J $editor)"], "args": ["--fence", "$(J $project)", "--inline-limit", "1000"] } ]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ctrl
$psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 3000 --tick-ms 0"
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
$proc = [System.Diagnostics.Process]::Start($psi)
$script:outTask = $null
$replies = @{}
function Ask($body, [string]$corr, [string]$to = 'editor:1') {
    $m = [ordered]@{ id = "m-$corr"; ts = 0; type = 'request'; from = 'console'; to = $to
                     corr = $corr; sid = $sid; authority = 'user'; payload = $body }
    $proc.StandardInput.WriteLine(($m | ConvertTo-Json -Compress -Depth 12))
    $proc.StandardInput.Flush()
    $deadline = [DateTime]::UtcNow.AddSeconds(25)
    while ([DateTime]::UtcNow -lt $deadline) {
        if ($null -eq $script:outTask) { $script:outTask = $proc.StandardOutput.ReadLineAsync() }
        if (-not $script:outTask.Wait(300)) { continue }
        $line = $script:outTask.Result
        $script:outTask = $null
        if ($null -eq $line -or $line.Trim() -eq '') { continue }
        $msg = $line | ConvertFrom-Json
        if ($msg.type -eq 'result' -and $msg.corr -eq $corr) { $replies[$corr] = $msg; return $msg }
    }
    return $null
}

Write-Host '== 打开那个大文件，读一页 ==' -ForegroundColor Cyan
$open = Ask @{ tool = 'open'; args = @{ path = 'big.txt' } } 'o'
$id = $open.payload.instance_id
$p1 = Ask @{ tool = 'read'; args = @{ instance_id = $id } } 'p1'
$got = $p1.payload.text
$cursor = $p1.payload.next_offset
$pages = 1
while ($null -ne $cursor -and $pages -lt 40) {
    $corr = "p" + ($pages + 1)
    $null = Ask @{ tool = 'read'; args = @{ instance_id = $id; offset = $cursor } } $corr
    $got += $replies[$corr].payload.text
    $cursor = $replies[$corr].payload.next_offset
    $pages++
}
$explicit = Ask @{ tool = 'read'; args = @{ instance_id = $id; offset = 0; limit = 200 } } 'e1'
$proc.StandardInput.Close()
$proc.WaitForExit(8000) | Out-Null
$et = $proc.StandardError.ReadToEndAsync(); $et.Wait(4000) | Out-Null

$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
Write-Host '== 判据 ==' -ForegroundColor Cyan
Check '1. 默认那次读**只给了一页**（不是把整个文件倒过来）' {
    $p1.payload.text.Length -le 1000 -and $p1.payload.bytes -gt 10000
}
Check '2. 而且**告诉你怎么接着读**（`next_offset` + 一句 why）' {
    $null -ne $p1.payload.next_offset -and $p1.payload.why -like '*分页*' -and `
        $p1.payload.why -like "*offset: $($p1.payload.next_offset)*"
}
Check '3. **`spilled_to` 是 null，而且话说清了**（全文就是那个文件，不需要溢写副本）' {
    $null -eq $p1.payload.spilled_to -and $p1.payload.why -like '*不需要溢写副本*'
}
Check '4. **分页拼回来 == 磁盘上那个文件**（这是最要紧的一条）' { $got -eq $original }
Check '5. 显式给 `limit` 就按它给' {
    $explicit.payload.text.Length -le 200 -and $explicit.payload.text.Length -gt 0
}
Check '6. 400 行一行不少' {
    (@($got -split "`r?`n" | Where-Object { $_ -like 'LINE-*' }).Count) -eq 400
}

Write-Host ''
Write-Host '== 第一页的说明（原话）==' -ForegroundColor Cyan
Write-Host "  $($p1.payload.why)"
Write-Host ''
Write-Host '== 规模 ==' -ForegroundColor Cyan
Write-Host ("  文件 {0} 字节，分 {1} 页读完，拼回来 {2} 字节" -f $original.Length, $pages, $got.Length)
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "工作目录留在：$work"
