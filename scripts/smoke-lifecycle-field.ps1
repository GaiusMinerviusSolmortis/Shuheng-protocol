# **`lifecycle` 真的会执行了**：装配的时机由它决定。
#
#   pwsh -File scripts/smoke-lifecycle-field.ps1
#
# 这个字段在文档里挂了很久，一直是"声明了但没人执行"。现在它的落地含义是**拉起时机**：
#
#   * `per_session`（默认）—— 会话装配时就拉起来；
#   * `per_call` —— **装配时不拉**，第一次有请求指向它才拉；
#   * `resident`（跨会话常驻）—— **还没实现，闸门会拦**（声明了没人执行的东西不许声明）。
#
# ⚠️ 它**不负责"用完就收"** —— §5.20 定过不做自动回收（收只走四条确定路径）。
# 所以别把它读成"按调用生命周期管理"，它管的是**什么时候起**。
#
# 这条开关解决的问题很实在：**原来"开会话"等于"把表里每个服务都拖起来"**，
# 哪怕这次会话根本用不到它们。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$echo = Join-Path $root 'target\debug\harness-echo.exe'
function J([string]$p) { return ($p -replace '\\', '\\') }
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-life2-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $work -Force | Out-Null
$sid = 's-life2'
$dataDir = Join-Path $work 'data'
$servicesPath = Join-Path $work 'services.json'
# 两个 mock：一个开会话时就起，一个声明"第一次用到才起"
@"
[
  { "kind": "echo", "id": "echo:eager", "role": "tool", "cmd": ["$(J $echo)"] },
  { "kind": "echo", "id": "echo:lazy",  "role": "tool", "cmd": ["$(J $echo)"],
    "lifecycle": "per_call" }
]
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
function Ask($body, [string]$corr, [string]$to = 'ctrl') {
    $m = [ordered]@{ id = "m-$corr"; ts = 0; type = 'request'; from = 'console'; to = $to
                     corr = $corr; sid = $sid; authority = 'user'; payload = $body }
    $proc.StandardInput.WriteLine(($m | ConvertTo-Json -Compress -Depth 12))
    $proc.StandardInput.Flush()
    $deadline = [DateTime]::UtcNow.AddSeconds(20)
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

Write-Host '== 开会话之后先看一眼：谁起来了 ==' -ForegroundColor Cyan
Start-Sleep -Milliseconds 1500
$before = Ask @{ op = 'list' } 'l1'
$idsBefore = @($before.payload.data.instances | ForEach-Object { $_.id })
Write-Host '== 现在去用那个"按需"的服务 ==' -ForegroundColor Cyan
$call = Ask @{ tool = 'ping'; args = @{} } 'p1' 'echo:lazy'
Start-Sleep -Milliseconds 800
$after = Ask @{ op = 'list' } 'l2'
$idsAfter = @($after.payload.data.instances | ForEach-Object { $_.id })
$proc.StandardInput.Close()
$proc.WaitForExit(8000) | Out-Null
$err = $proc.StandardError.ReadToEnd()

$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}
Write-Host '== 判据 ==' -ForegroundColor Cyan
Check '1. **`per_session` 的那个：开会话就起来了**' { $idsBefore -contains 'echo:eager' }
Check '2. **`per_call` 的那个：开会话时没起**（这是这条开关的全部意义）' {
    $idsBefore -notcontains 'echo:lazy'
}
Check '3. 中控说清了它为什么不拉' { $err -like '*echo:lazy*per_call*装配时不拉*' }
Check '4. **第一次调用它 ⇒ 按需拉起**，中控也说明了' {
    $err -like '*按需拉起 echo:lazy*'
}
Check '5. 而且**那条调用没失败**（拉起之后照常答上了）' {
    $call -and $call.payload.effect -eq 'confirmed' -and $call.payload.data.pong -eq $true
}
Check '6. 用过之后它就在图上了' { $idsAfter -contains 'echo:lazy' }
Check '7. **部署的决定和它自述的并排看得见**（不一致不是错，但不许藏起来）' {
    $lazy = $after.payload.data.instances | Where-Object { $_.id -eq 'echo:lazy' } | Select-Object -First 1
    $eager = $after.payload.data.instances | Where-Object { $_.id -eq 'echo:eager' } | Select-Object -First 1
    # lazy：表里写了 per_call，服务自述也是 per_call ⇒ 一致
    $lazy.lifecycle -eq 'per_call' -and $lazy.lifecycle_declared -eq 'per_call' -and `
        $lazy.lifecycle_mismatch -eq $false -and `
        # eager：表没写（默认 per_session），而 echo 自述是 per_call ⇒ 一眼看得出差在哪
        $eager.lifecycle -eq 'per_session' -and $eager.lifecycle_declared -eq 'per_call' -and `
        $eager.lifecycle_mismatch -eq $true
}

Write-Host ''
Write-Host '== 中控那两句（原话）==' -ForegroundColor Cyan
$err -split "`n" | Where-Object { $_ -like '*per_call*' -or $_ -like '*按需拉起*' } |
    ForEach-Object { Write-Host "  $($_.Trim())" }
Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "工作目录留在：$work"
