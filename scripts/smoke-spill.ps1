# **背压**：输出比消费快得多的时候，谁去扛（§5.17）。
#
#   pwsh -File scripts/smoke-spill.ps1
#
# 这条在文档里挂了两轮，卡点是两个数和一个死循环：
#
#   * **数**：一次结果里最多内联多少？→ **50 000 B**（§5.4 查过的那批数据里
#     dsh 的 `maxInlineBytes`；而且它是**部署选择**，`--inline-limit` 可改）；
#   * **数**：内存缓冲留多少？→ **1 MiB**（够装下正常命令，扛不住 `yes`）；
#   * **死循环**：全文要是只能靠"再跑一条命令"读，那读全文又变成一次终端调用。
#     → 解法是**游标读**：全文留在实例里（内存 + 溢出文件），`read` 跨两段按字节读。
#     于是"想全看"是多读几页，不是再跑命令。
#
# 最要紧的一条判据在最后：**把分页读回来的字节拼起来，必须和原文一字不差。**
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$term = Join-Path $root 'target\debug\harness-term.exe'
function J([string]$p) { return ($p -replace '\\', '\\') }
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-spill-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
$spillDir = Join-Path $work 'spill'
New-Item -ItemType Directory -Path $project -Force | Out-Null
$sid = 's-spill'
$dataDir = Join-Path $work 'data'
# **故意把两个数都调小**，几百字节的输出就能把两条路都走一遍
$servicesPath = Join-Path $work 'services.json'
@"
[ { "kind": "term", "id": "term:1", "role": "tool",
    "cmd": ["$(J $term)"],
    "args": ["--fence", "$(J $project)",
             "--inline-limit", "1000", "--memory-cap", "700",
             "--spill-dir", "$(J $spillDir)"] } ]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ctrl
$psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 4000 --tick-ms 0"
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
$proc = [System.Diagnostics.Process]::Start($psi)
$script:outTask = $null
$replies = @{}
function Ask($body, [string]$corr, [string]$to = 'term:1') {
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

Write-Host '== 开一个终端，跑一条吐很多字的命令 ==' -ForegroundColor Cyan
$open = Ask @{ tool = 'open'; args = @{ path = '.'; purpose = '背压验收' } } 'o'
$id = $open.payload.instance_id
# 600 行 × 30 字节 ≈ 18 KB：超过内联上限（1000），也超过内存上限（700）
$cmd = 'for /L %i in (1,1,600) do @echo LINE-%i-XXXXXXXXXXXXXXXXXXXX'
$run = Ask @{ tool = 'run'; args = @{ instance_id = $id; cmd = $cmd } } 'r1'
$out = $run.payload.output

Write-Host '== 再把全文按游标读回来 ==' -ForegroundColor Cyan
$null = Ask @{ tool = 'read'; args = @{ instance_id = $id; since = 0; limit = 1000 } } 'p1'
$total = $replies['p1'].payload.bytes
$got = $replies['p1'].payload.text
$cursor = $replies['p1'].payload.next_cursor
$pages = 1
while ($null -ne $cursor -and $pages -lt 40) {
    $corr = "p" + ($pages + 1)
    $null = Ask @{ tool = 'read'; args = @{ instance_id = $id; since = $cursor; limit = 1000 } } $corr
    $got += $replies[$corr].payload.text
    $cursor = $replies[$corr].payload.next_cursor
    $pages++
}
$proc.StandardInput.Close()
$proc.WaitForExit(8000) | Out-Null
$et = $proc.StandardError.ReadToEndAsync(); $et.Wait(4000) | Out-Null
$spillFiles = @(Get-ChildItem -Recurse -File $spillDir -ErrorAction SilentlyContinue)
$lines = @($got -split "`r?`n" | Where-Object { $_ -like 'LINE-*' })

$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
Write-Host '== 判据 ==' -ForegroundColor Cyan
Check '1. 命令真跑起来了，而且吐了很多字' {
    $run.payload.effect -eq 'confirmed' -and $run.payload.bytes -gt 3000
}
Check '2. **结果被压到内联上限以内**（不是把 18 KB 全塞进来）' { $out.Length -le 1000 }
Check '3. 头尾都在：第一行和最后一行都看得见（这才是"头尾 + 省略"）' {
    $out -like '*LINE-1-*' -and $out -like '*LINE-600-*'
}
Check '4. 省略说明里带着**省略了多少字节**，以及**怎么读全文**' {
    $out -like '*省略*字节*' -and $out -like '*read*since*'
}
Check '5. 结果里带 `spilled_to`（全文落在哪个文件里）' {
    $run.payload.spilled_to -and $run.payload.spilled_to -like '*spill*'
}
Check '6. 那个文件**真的有东西**（内存缓冲确实溢出去了）' {
    $spillFiles.Count -ge 1 -and ($spillFiles | Measure-Object -Property Length -Sum).Sum -gt 0
}
Check '7. **分页读回来的字节数 == 缓冲总字节数**（一页都没漏、也没重复）' {
    $got.Length -eq $total
}
Check '8. **拼起来就是原文**：600 行一行不少' { $lines.Count -eq 600 }
Check '9. 读回来的东西**没有替换符**（跨段的字符边界没被切坏）' {
    $got -notlike "*$([char]0xFFFD)*"
}

Write-Host ''
Write-Host '== 那次 run 的结果（前 200 字）==' -ForegroundColor Cyan
Write-Host ("  " + $out.Substring(0, [Math]::Min(200, $out.Length)).Replace("`r", "").Replace("`n", " / "))
Write-Host ''
Write-Host '== 分页读回来的规模 ==' -ForegroundColor Cyan
Write-Host ("  缓冲总字节 {0}，分 {1} 页读完，拼回来 {2} 字节、{3} 行" -f $total, $pages, $got.Length, $lines.Count)
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "工作目录留在：$work"
