# 图上**第三层**的验收：工具实例。
#
#   pwsh -File scripts/smoke-graph.ps1
#
# 在这之前，`t-1` / `ed-1` 这些真在干活的东西**在中控那里一个节点都没有** ——
# 服务把事件发出来了、中控也记进日志了，但没建节点。
# 后果很具体：§12.7 那张"节点与连线"图**画不出来**，
# 而且"针对某个实例的敕令"没有对象可指。
#
# 这个脚本就问中控一个问题：**你现在这张图上有什么？**（`{"op":"list"}`）
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$term = Join-Path $root 'target\debug\harness-term.exe'
$editor = Join-Path $root 'target\debug\harness-editor.exe'
function J([string]$p) { return ($p -replace '\\', '\\') }
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-graph-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null
Set-Content -LiteralPath (Join-Path $project 'main.rs') -Value 'fn main() {}' -Encoding utf8
$sid = 's-graph'
$dataDir = Join-Path $work 'data'
$servicesPath = Join-Path $work 'services.json'
@"
[
  { "kind": "term",   "id": "term:1",   "role": "tool",
    "cmd": ["$(J $term)"],   "args": ["--fence", "$(J $project)"] },
  { "kind": "editor", "id": "editor:1", "role": "tool",
    "cmd": ["$(J $editor)"], "args": ["--fence", "$(J $project)"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ctrl
$psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 5000"
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
$proc = [System.Diagnostics.Process]::Start($psi)
$script:outTask = $null
$script:seq = 0
$script:replies = @{}
function NextLine([int]$timeoutMs) {
    if ($null -eq $script:outTask) { $script:outTask = $proc.StandardOutput.ReadLineAsync() }
    if (-not $script:outTask.Wait($timeoutMs)) { return $null }
    $line = $script:outTask.Result
    $script:outTask = $null
    return $line
}
function Write-Env([string]$to, [string]$corr, [hashtable]$payload) {
    $script:seq++
    $msg = [ordered]@{
        id = "m-$($script:seq)"; ts = 0; type = 'request'; from = 'console'
        to = $to; corr = $corr; sid = $sid; authority = 'user'; payload = $payload
    }
    $proc.StandardInput.WriteLine(($msg | ConvertTo-Json -Compress -Depth 12))
    $proc.StandardInput.Flush()
}
function Wait([string]$corr, [int]$timeoutMs = 20000) {
    if ($script:replies.ContainsKey($corr)) { return $script:replies[$corr] }
    $deadline = [DateTime]::UtcNow.AddMilliseconds($timeoutMs)
    while ([DateTime]::UtcNow -lt $deadline) {
        $left = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
        $line = NextLine $left
        if ($null -eq $line) { continue }
        if ($line.Trim() -eq '') { continue }
        $m = $line | ConvertFrom-Json
        if ($m.type -eq 'result') { $script:replies[$m.corr] = $m }
        if ($m.type -eq 'result' -and $m.corr -eq $corr) { return $m }
    }
    throw "等 $corr 超时"
}
function Ask([string]$to, [string]$corr, [hashtable]$payload) { Write-Env $to $corr $payload; return Wait $corr }
function Graph([string]$corr) { return (Ask 'ctrl' $corr @{ op = 'list' }).payload.data }
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
Write-Host '== 判据 ==' -ForegroundColor Cyan
$g0 = Graph 'g-0'
Check '1. 起点：图上有服务节点，但一个工具实例都没有' {
    $g0.tool_instances.Count -eq 0 -and (($g0.instances | Where-Object { $_.id -eq 'term:1' }) -ne $null)
}
$null = Ask 'term:1' 't-open' @{ tool = 'open'; args = @{ path = '.'; purpose = '跑测试' } }
$g1 = Graph 'g-1'
$node = $g1.tool_instances | Where-Object { $_.id -eq 't-1' }
Check '2. 开了终端之后图上**多了一个节点**，而且带归属、状态、用途' {
    # `service` = 它挂在哪个服务上；`owner` = **谁开的这个实例**（2026-09-19 加）
    $node -and $node.service -eq 'term:1' -and $node.state -eq 'idle' -and `
        $node.purpose -eq '跑测试' -and $node.owner -like 'console@*'
}
Check '3. 那条边也在（服务 --owns--> 实例）' {
    $e = $g1.edges | Where-Object { $_.to -eq 'term:1/t-1' }
    $e -and $e.from -eq 'term:1' -and $e.kind -eq 'owns'
}
Check '4. 节点上带着路径（图上点开要看的东西）' { $node.path -like '*project*' }
$runLine = '{"id":"m-run","ts":0,"type":"request","from":"console","to":"term:1","corr":"t-run","sid":"' + $sid + '","authority":"user","payload":{"tool":"run","args":{"instance_id":"t-1","cmd":"ping -n 4 127.0.0.1"}}}'
$proc.StandardInput.WriteLine($runLine)
$proc.StandardInput.Flush()
Start-Sleep -Milliseconds 700
$g2 = Graph 'g-2'
$running = $g2.tool_instances | Where-Object { $_.id -eq 't-1' }
Check '5. 命令跑着的时候节点状态是 running（图上的颜色靠它）' { $running -and $running.state -eq 'running' }
$r = Wait 't-run' 20000
$g3 = Graph 'g-3'
$idle = $g3.tool_instances | Where-Object { $_.id -eq 't-1' }
Check '6. 跑完之后回到 idle' { $r.payload.effect -eq 'confirmed' -and $idle.state -eq 'idle' }
$null = Ask 'editor:1' 'e-open' @{ tool = 'open'; args = @{ path = 'main.rs' } }
$g4 = Graph 'g-4'
Check '7. 换一个域也一样：编辑器实例出现在图上（owner 不同）' {
    (($g4.tool_instances | Where-Object { $_.id -eq 'ed-1' -and $_.service -eq 'editor:1' }) -ne $null) -and `
        $g4.tool_instances.Count -eq 2
}
$null = Ask 'term:1' 't-close' @{ tool = 'close'; args = @{ instance_id = 't-1' } }
$g5 = Graph 'g-5'
Check '8. 关掉之后节点**从图上消失**（不是留个灰点）' {
    ($g5.tool_instances | Where-Object { $_.id -eq 't-1' }).Count -eq 0 -and `
        ($g5.tool_instances | Where-Object { $_.id -eq 'ed-1' }).Count -eq 1
}
$proc.StandardInput.Close()
$proc.WaitForExit(8000) | Out-Null
$et = $proc.StandardError.ReadToEndAsync(); $et.Wait(4000) | Out-Null
Write-Host ''
Write-Host '== 中控的 stderr ==' -ForegroundColor Cyan
if ($et.IsCompleted) { foreach ($l in ($et.Result -split "`n")) { if ($l.Trim() -ne '') { Write-Host "  $l" } } }
Write-Host ''
Check '9. 中控的 stderr 里留着图的变更轨迹（可审计）' {
    $et.IsCompleted -and $et.Result -like '*图：term:1 下多了*'
}
Write-Host ''
Write-Host '== 最后一次问图 ==' -ForegroundColor Cyan
$g5.tool_instances | ForEach-Object { Write-Host ("  节点 {0,-22} owner={1,-10} state={2,-8} {3}" -f $_.key, $_.owner, $_.state, $_.purpose) }
$g5.edges | ForEach-Object { Write-Host ("  边   {0} --{1}--> {2}" -f $_.from, $_.kind, $_.to) }
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    Write-Host "工作目录：$work"
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "工作目录留在：$work"
