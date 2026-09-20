# **手册** 与 **能力校验** —— 两个都落在"执行侧"，不在中控。
#
#   pwsh -File scripts/smoke-help-grant.ps1
#
# 这一轮补的是 README 那张差距表里的前两条，而且补法是同一个原则：
# **责任向下传导**。
#
#   * `help` —— 服务**自己写**（手写、分章），中控不参与。
#     规范 §11.1 说得很直白：从接口自动生成的文档"说得没错但帮不上忙"。
#   * `capabilities` —— 中控**签发**，服务**自己校验**（§7："工具只校验手里的凭证
#     够不够干这件事"）。所以"改一条能力要求"不用动中控。
#   * `route` —— 结果里的这个字段由**服务自己报**（非 UI 服务为 null，§11.2.2）。
#
# 两个场景：
#   A 给智能体**零能力**：help 还能用（不需要能力），run 被**服务**拒
#   B 给它 `proc.exec`：同一个 run 就能用了
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$term = Join-Path $root 'target\debug\harness-term.exe'
$agentBin = Join-Path $root 'target\debug\harness-agent.exe'
function J([string]$p) { return ($p -replace '\\', '\\') }
function Run-Scenario([string]$tag, [string]$bindingsJson, [string]$scriptBody) {
    $work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-hg-$tag-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
    $project = Join-Path $work 'project'
    New-Item -ItemType Directory -Path $project -Force | Out-Null
    $sid = "s-$tag"
    $dataDir = Join-Path $work 'data'
    $scriptPath = Join-Path $work 'script.ndjson'
    Set-Content -LiteralPath $scriptPath -Value $scriptBody -Encoding utf8
    $servicesPath = Join-Path $work 'services.json'
    @"
[
  { "kind": "term", "id": "term:1", "role": "tool", "wants_fence": true,
    "cmd": ["$(J $term)"] },
  { "kind": "agent", "id": "agent:dada", "role": "agent", "bindings": $bindingsJson,
    "cmd": ["$(J $agentBin)"],
    "args": ["--persona", "达达", "--script", "$(J $scriptPath)"] }
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
    function NextLine([int]$timeoutMs) {
        if ($null -eq $script:outTask) { $script:outTask = $proc.StandardOutput.ReadLineAsync() }
        if (-not $script:outTask.Wait($timeoutMs)) { return $null }
        $line = $script:outTask.Result
        $script:outTask = $null
        return $line
    }
    $task = [ordered]@{
        id = 'm-1'; ts = 0; type = 'request'; from = 'console'; to = 'agent:dada'
        corr = 'c-1'; sid = $sid; authority = 'user'; payload = @{ task = '先看看手册，再试着干活' }
    }
    $proc.StandardInput.WriteLine(($task | ConvertTo-Json -Compress -Depth 12))
    $proc.StandardInput.Flush()
    $deadline = [DateTime]::UtcNow.AddSeconds(40)
    $answer = $null
    while ([DateTime]::UtcNow -lt $deadline) {
        $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
        $line = NextLine $left
        if ($null -eq $line) { continue }
        if ($line.Trim() -eq '') { continue }
        $m = $line | ConvertFrom-Json
        if ($m.type -eq 'result' -and $m.corr -eq 'c-1') { $answer = $m; break }
    }
    $proc.StandardInput.Close()
    $proc.WaitForExit(8000) | Out-Null
    $et = $proc.StandardError.ReadToEndAsync(); $et.Wait(4000) | Out-Null
    $logPath = Join-Path (Join-Path (Join-Path $dataDir 'sessions') $sid) 'log.jsonl'
    $records = @()
    if (Test-Path $logPath) { $records = @(Get-Content -LiteralPath $logPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json }) }
    return [pscustomobject]@{ work=$work; sid=$sid; records=$records; answer=$answer
        stderr = $(if ($et.IsCompleted) { $et.Result } else { '' }) }
}
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
$scriptA = @'
{"step":"tool","calls":[{"to":"term:1","tool":"help","args":{}}]}
{"step":"tool","calls":[{"to":"term:1","tool":"help","args":{"section":"cursor"}}]}
{"step":"tool","calls":[{"to":"term:1","tool":"run","args":{"instance_id":"t-1","cmd":"echo 我该被拦住"}}]}
{"step":"text","text":"手册看到了；run 被拦住了。"}
'@
Write-Host '== 场景 A：给它**零能力** ==' -ForegroundColor Cyan
$A = Run-Scenario 'A' '[{"instance":"term:1","capabilities":[]}]' $scriptA
$helpList = $A.records | Where-Object { $_.type -eq 'result' -and $_.from -eq 'term:1' -and $_.payload.data.sections } | Select-Object -First 1
$helpOne  = $A.records | Where-Object { $_.type -eq 'result' -and $_.from -eq 'term:1' -and $_.payload.data.section -eq 'cursor' } | Select-Object -First 1
$denied   = $A.records | Where-Object { $_.type -eq 'result' -and $_.from -eq 'term:1' -and $_.payload.code -eq 'refused' -and $_.payload.why -like '*需要的能力你没有*' } | Select-Object -First 1
Check '1. **手册是分章的**：不带参数给章节清单（不是一次全倒）' {
    $helpList -and $helpList.payload.data.sections.Count -ge 4 -and $helpList.payload.data.why -like '*渐进*'
}
Check '2. 带 section 就取那一章，正文是服务自己写的' {
    $helpOne -and $helpOne.payload.data.text -like '*游标*'
}
Check '3. **零能力也能调手册** —— 它不需要能力' {
    $helpList.payload.effect -eq 'confirmed'
}
Check '4. **run 被服务自己拒了**（不是中控拦的）' {
    $denied -ne $null
}
Check '5. 拒绝里说清差哪条、有什么、下一步' {
    $denied.payload.why -like '*proc.exec*' -and $denied.payload.granted -like '*没有*' -and `
        $denied.payload.remedy -like '*中控签发*'
}
Check '6. 结果里带着 route 字段（非 UI 服务为 null，§11.2.2）' {
    $helpList.payload.PSObject.Properties.Name -contains 'route' -and $null -eq $helpList.payload.route
}
Check '7. 智能体看到的名片里有 help（因为服务自己在 manifest 里声明了它）' {
    $p = ($A.records | Where-Object { $_.payload.kind -eq 'prompt_compiled' } | Select-Object -First 1).payload.system
    $p -like '*help*'
}
Write-Host ''
Write-Host '== 场景 B：给它 `proc.exec` ==' -ForegroundColor Cyan
$scriptB = @'
{"step":"tool","calls":[{"to":"term:1","tool":"open","args":{"path":".","purpose":"有授权之后"}}]}
{"step":"text","text":"开成了。"}
'@
$B = Run-Scenario 'B' '[{"instance":"term:1","capabilities":["proc.exec"]}]' $scriptB
$okOpen = $B.records | Where-Object { $_.type -eq 'result' -and $_.from -eq 'term:1' -and $_.payload.instance_id } | Select-Object -First 1
Check '8. 拿到 `proc.exec` 之后，同一个 open 就能用了' { $okOpen -ne $null }
Check '9. 那次调用的请求里带着中控签发的 granted' {
    $req = $B.records | Where-Object { $_.type -eq 'request' -and $_.from -eq 'agent:dada' -and $_.payload.tool -eq 'open' } | Select-Object -First 1
    $req -and ($req.payload.granted -contains 'proc.exec')
}
<#
  第 10 条是 **2026-09-20 审计**补的。它是那种"躺在日志里、没人看"的东西：

  服务拒绝时回的 `refuse_payload` 里有一个 `caller_kind`（"我认为你是谁"），
  而**没有任何一个工具的 `output_schema` 声明过它** —— 契约是闭合的，
  于是**每一次能力拒绝都在违反它自己声明的形状**，投递闸门每次都得贴一张
  `schema_violation`。这条判据把它钉死：拒得对不对是一回事，
  **拒绝的结果本身也得合形**是另一回事。
#>
Check '10. 能力拒绝的结果**也合它自己声明的形**（不许带 schema_violation）' {
    @($A.records | Where-Object { $_.payload.schema_violation }).Count -eq 0 -and `
        @($B.records | Where-Object { $_.payload.schema_violation }).Count -eq 0
}
Write-Host ''
Write-Host '== 场景 A 的那次拒绝 ==' -ForegroundColor Cyan
if ($denied) { Write-Host "  why:     $($denied.payload.why)"; Write-Host "  granted: $($denied.payload.granted)"; Write-Host "  remedy:  $($denied.payload.remedy)" }
Write-Host ''
Write-Host '== 手册的章节清单 ==' -ForegroundColor Cyan
if ($helpList) { $helpList.payload.data.sections | ForEach-Object { Write-Host "  $($_.id)  —  $($_.title)" } }
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$($A.work)"
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "工作目录留在：$($A.work)"
