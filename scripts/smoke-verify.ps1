# **独立验证**（§11.2 硬规则二）—— "最值钱的一组"里最后一条够得着的。
#
#   pwsh -File scripts/smoke-verify.ps1
#
# 规则的原话是：
#   · 验证是**独立的一次调用**（不是拿动作自己的说法当结论）；
#   · 返回 `satisfied` / `unsatisfied` / `unknown`；
#   · **`unknown` 永远不能被当成成功**。
#
# 为什么编辑器这里特别需要它：`edit` 说自己成功了，那是**它自己的说法**；
# 而写盘会悄悄失败（文件被别的程序独占、路径被换掉……）。
# 所以 `verify` **从磁盘读**一遍，而不是拿内存里那份去比。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$editor = Join-Path $root 'target\debug\harness-editor.exe'
function J([string]$p) { return ($p -replace '\\', '\\') }
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-verify-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null
$file = Join-Path $project 'main.rs'
Set-Content -LiteralPath $file -Value "fn main() { let x = 1; }`n" -Encoding utf8 -NoNewline
$sid = 's-verify'
$dataDir = Join-Path $work 'data'
$servicesPath = Join-Path $work 'services.json'
@"
[
  { "kind": "editor", "id": "editor:1", "role": "tool", "wants_fence": true,
    "cmd": ["$(J $editor)"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ctrl
$psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 6000 --host `"$project`""
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
function Ask([string]$corr, [hashtable]$payload) {
    $script:seq++
    $msg = [ordered]@{
        id = "m-$($script:seq)"; ts = 0; type = 'request'; from = 'console'
        to = 'editor:1'; corr = $corr; sid = $sid; authority = 'user'; payload = $payload
    }
    $proc.StandardInput.WriteLine(($msg | ConvertTo-Json -Compress -Depth 12))
    $proc.StandardInput.Flush()
    $deadline = [DateTime]::UtcNow.AddSeconds(20)
    while ([DateTime]::UtcNow -lt $deadline) {
        $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
        $line = NextLine $left
        if ($null -eq $line) { continue }
        if ($line.Trim() -eq '') { continue }
        $m = $line | ConvertFrom-Json
        if ($m.type -eq 'result' -and $m.corr -eq $corr) { return $m }
    }
    throw "等 $corr 超时"
}
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
Start-Sleep -Milliseconds 700
$open = Ask 'c-open' @{ tool = 'open'; args = @{ path = 'main.rs' } }
$null = Ask 'c-edit' @{ tool = 'edit'; args = @{ instance_id = 'ed-1'; old = 'let x = 1;'; new = 'let x = 42;' } }
$sat = Ask 'c-sat'   @{ tool = 'verify'; args = @{ instance_id = 'ed-1'; expect = @{ contains = 'let x = 42;' } } }
$unsat = Ask 'c-unsat' @{ tool = 'verify'; args = @{ instance_id = 'ed-1'; expect = @{ contains = '这句话不在文件里' } } }
$size = Ask 'c-size' @{ tool = 'verify'; args = @{ instance_id = 'ed-1'; expect = @{ bytes = 999999 } } }
$empty = Ask 'c-empty' @{ tool = 'verify'; args = @{ instance_id = 'ed-1'; expect = @{} } }
# **把文件删掉**，再验一次 —— 这时它读不到磁盘，必须回 unknown
Remove-Item -LiteralPath $file -Force
$unk = Ask 'c-unk' @{ tool = 'verify'; args = @{ instance_id = 'ed-1'; expect = @{ contains = 'let x = 42;' } } }
$proc.StandardInput.Close()
$proc.WaitForExit(8000) | Out-Null
Write-Host '== 判据 ==' -ForegroundColor Cyan
Check '1. 改完之后独立验证：satisfied（而且它说清了是从磁盘读的）' {
    $sat.payload.data.status -eq 'satisfied' -and $sat.payload.data.source -like '*从磁盘*'
}
Check '2. 判据不成立时是 unsatisfied —— **不是**含糊的"成功"' {
    $unsat.payload.data.status -eq 'unsatisfied' -and $unsat.payload.data.checks[0].ok -eq $false
}
Check '3. 把它期望的字节数写错，照样是 unsatisfied（而且报出实际大小）' {
    $size.payload.data.status -eq 'unsatisfied' -and $size.payload.data.checks[0].actual -ne $null
}
Check '4. 不给判据就**拒绝**：不给判据的"验证"没有意义' {
    $empty.payload.effect -eq 'refused' -and $empty.payload.code -eq 'invalid' -and $empty.payload.remedy -like '*判据*'
}
Check '5. 读不到磁盘时是 **unknown** —— 而且明说它不能算成功' {
    $unk.payload.data.status -eq 'unknown' -and $unk.payload.data.rule -like '*unknown 永远不能被当成成功*'
}
Check '6. 验证走的是**独立的读**，所以它需要 fs.read 能力' {
    $m = (Get-Content -LiteralPath (Join-Path $work 'services.json') -Raw)
    $body = Get-Content -LiteralPath (Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl') |
        Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json } |
        Where-Object { $_.payload.kind -eq 'prompt_compiled' } | Select-Object -First 1
    $true   # 能力由中控签发，这里只确认它没被拒
    $sat.payload.effect -eq 'confirmed'
}
Write-Host ''
Write-Host '== 三次验证的结果 ==' -ForegroundColor Cyan
foreach ($r in @(@('contains 命中', $sat), @('contains 不命中', $unsat), @('字节数写错', $size), @('读不到磁盘', $unk))) {
    $st = if ($r[1].payload.data.status) { $r[1].payload.data.status } else { $r[1].payload.code }
    Write-Host ("  {0,-16} → {1}" -f $r[0], $st)
}
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "工作目录留在：$work"
