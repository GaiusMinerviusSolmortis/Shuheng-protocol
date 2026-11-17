# **执行账本的最小故障验收**：中控在效果进行中死掉，重启后不能假装没做。
#
#   pwsh -File scripts/smoke-runtime.ps1
#
# 这条验收故意绕开模型：console 直接调用 echo 的 `slow`，等账本写出
# `effect_started` 后硬杀中控。新中控打开同一个会话时，必须把旧效果结算成
# `unverifiable`，并同时收口 attempt / intent。它验证的是“消息日志之外，
# 系统是否知道现实动作可能已经发生”，不是某条错误文案。
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$echo = Join-Path $root 'target\debug\harness-echo.exe'
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-runtime-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $work | Out-Null
$dataDir = Join-Path $work 'data'
$sid = 's-runtime'
$corr = 'c-runtime-1'
$servicesPath = Join-Path $work 'services.json'
@"
[
  { "kind": "echo", "cmd": ["$(($echo -replace '\\','\\'))"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8
$requestPath = Join-Path $work 'request.ndjson'
@"
{"id":"m-runtime-1","ts":0,"type":"request","from":"console","to":"echo:1","corr":"$corr","sid":"$sid","authority":"user","payload":{"tool":"slow","args":{"ms":8000}}}
"@ | Set-Content -LiteralPath $requestPath -Encoding utf8
$ledger = Join-Path $dataDir 'runtime\execution\s-runtime\execution.jsonl'
function Start-Ctrl([string]$inputPath, [string]$tag) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $ctrl
    $psi.Arguments = "--sid $sid --data-dir `"$dataDir`" --services `"$servicesPath`" --drain-ms 1000"
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $psi.StandardInputEncoding = [System.Text.UTF8Encoding]::new($false)
    $p = [System.Diagnostics.Process]::Start($psi)
    $outTask = $p.StandardOutput.ReadToEndAsync()
    $errTask = $p.StandardError.ReadToEndAsync()
    $lines = Get-Content -LiteralPath $inputPath
    foreach ($line in $lines) { $p.StandardInput.WriteLine($line) }
    $p.StandardInput.Flush()
    return [pscustomobject]@{ Proc = $p; Out = $outTask; Err = $errTask; Tag = $tag }
}
function Wait-For([scriptblock]$cond, [int]$timeoutMs = 8000) {
    $deadline = [DateTime]::UtcNow.AddMilliseconds($timeoutMs)
    while ([DateTime]::UtcNow -lt $deadline) {
        if (& $cond) { return $true }
        Start-Sleep -Milliseconds 50
    }
    return $false
}

$first = Start-Ctrl $requestPath 'first'
$sawStart = Wait-For {
    (Test-Path -LiteralPath $ledger) -and
        ((Get-Content -LiteralPath $ledger -Raw) -like '*"kind":"effect_started"*') -and
        ((Get-Content -LiteralPath $ledger -Raw) -like "*$corr*")
}
$beforeKill = if (Test-Path -LiteralPath $ledger) {
    @(Get-Content -LiteralPath $ledger | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json })
} else { @() }
if (-not $first.Proc.HasExited) { Stop-Process -Id $first.Proc.Id -Force }
$first.Proc.WaitForExit(5000) | Out-Null

$emptyPath = Join-Path $work 'empty.ndjson'
Set-Content -LiteralPath $emptyPath -Value '' -Encoding utf8
$second = Start-Ctrl $emptyPath 'second'
$second.Proc.StandardInput.Close()
$second.Proc.WaitForExit(8000) | Out-Null
$after = @(Get-Content -LiteralPath $ledger | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json })

$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}

$startedBefore = @($beforeKill | Where-Object { $_.kind -eq 'effect_started' -and $_.effect_id -eq $corr })
Check '1. 目标进程动手之前，效果已经进了执行账' {
    $startedBefore.Count -eq 1 -and $startedBefore[0].target -eq 'echo:1' -and
        $startedBefore[0].tool -eq 'slow' -and $startedBefore[0].arguments_digest
}
Check '2. 硬杀发生前，这个效果还没有被伪装成已结算' {
    @($beforeKill | Where-Object { $_.kind -eq 'effect_settled' -and $_.effect_id -eq $corr }).Count -eq 0
}
Check '3. 重启后把它标成 unverifiable，而不是 failed/noop/confirmed' {
    $e = @($after | Where-Object { $_.kind -eq 'effect_settled' -and $_.effect_id -eq $corr })
    $e.Count -eq 1 -and $e[0].state -eq 'unverifiable' -and $e[0].code -eq 'controller_restarted'
}
Check '4. 恢复同时收口执行尝试，不留下悬挂的 attempt' {
    @($after | Where-Object { $_.kind -eq 'attempt_finished' -and $_.state -eq 'failed' }).Count -eq 1
}
Check '5. 恢复同时收口父意图' {
    @($after | Where-Object { $_.kind -eq 'intent_finished' -and $_.state -eq 'failed' }).Count -eq 1
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
