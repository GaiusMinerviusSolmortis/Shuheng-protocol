# 阶段 2 的验收脚本：**第一个真工具实例 —— 编辑器**。
#
#   pwsh -File scripts/smoke-editor.ps1
#
# 它验的是这套设计里最新、最没先例可抄的两件事（README 阶段 2）：
#   路径围栏（§7.1）与写互斥（§5.15）；回滚（§5.24 的 L1）是顺带验的。
#
# 判据 1~8 是**必须通过**的。
# 判据 9 是一条**已知空洞的特征化断言** —— 它现在会通过，因为洞还在；
# 等到洞被补上，它会失败，那就是提醒你把它改成"应当被挡住"。

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$ctrl = Join-Path $root 'target\debug\harness-ctrl.exe'
$editor = Join-Path $root 'target\debug\harness-editor.exe'

Write-Host '== 构建 ==' -ForegroundColor Cyan
cargo build --manifest-path (Join-Path $root 'Cargo.toml') --quiet
if (-not (Test-Path $editor)) { throw "产物不在：$editor" }

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("harness-editor-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$project = Join-Path $work 'project'
New-Item -ItemType Directory -Path $project -Force | Out-Null

$original = @'
fn main() {
    let x = 1;
    println!("{}", x);
}
'@
$filePath = Join-Path $project 'main.rs'
$original | Set-Content -LiteralPath $filePath -Encoding utf8 -NoNewline

$sid = 's-ed'
$servicesPath = Join-Path $work 'services.json'
@"
[
  { "kind": "editor", "id": "editor:1", "cmd": ["$(($editor -replace '\\','\\'))"], "args": ["--fence", "$($project -replace '\\','\\')"] },
  { "kind": "editor", "id": "editor:2", "cmd": ["$(($editor -replace '\\','\\'))"], "args": ["--fence", "$($project -replace '\\','\\')"] }
]
"@ | Set-Content -LiteralPath $servicesPath -Encoding utf8

$inputPath = Join-Path $work 'in.ndjson'
@'
{"id":"m-01","ts":0,"type":"request","from":"console","to":"editor:1","corr":"c-01","sid":"s-ed","authority":"user","payload":{"tool":"ping"}}
{"id":"m-02","ts":0,"type":"request","from":"console","to":"editor:1","corr":"c-02","sid":"s-ed","authority":"user","payload":{"tool":"open","args":{"path":"main.rs"}}}
{"id":"m-03","ts":0,"type":"request","from":"console","to":"editor:1","corr":"c-03","sid":"s-ed","authority":"user","payload":{"tool":"open","args":{"path":"main.rs"}}}
{"id":"m-04","ts":0,"type":"request","from":"console","to":"editor:1","corr":"c-04","sid":"s-ed","authority":"user","payload":{"tool":"open","args":{"path":"../escape.txt"}}}
{"id":"m-05","ts":0,"type":"request","from":"console","to":"editor:1","corr":"c-05","sid":"s-ed","authority":"user","payload":{"tool":"open","args":{"path":"new.txt"}}}
{"id":"m-06","ts":0,"type":"request","from":"console","to":"editor:1","corr":"c-06","sid":"s-ed","authority":"user","payload":{"tool":"open","args":{"path":"new.txt","create":true}}}
{"id":"m-06b","ts":0,"type":"request","from":"console","to":"editor:1","corr":"c-06b","sid":"s-ed","authority":"user","payload":{"tool":"edit","args":{"instance_id":"ed-2","old":"","new":"PERMANENT\n"}}}
{"id":"m-07","ts":0,"type":"request","from":"console","to":"editor:1","corr":"c-07","sid":"s-ed","authority":"user","payload":{"tool":"edit","args":{"instance_id":"ed-1","old":"let x = 1;","new":"let x = 42;"}}}
{"id":"m-08","ts":0,"type":"request","from":"console","to":"editor:1","corr":"c-08","sid":"s-ed","authority":"user","payload":{"tool":"read","args":{"instance_id":"ed-1"}}}
{"id":"m-09","ts":0,"type":"request","from":"console","to":"editor:1","corr":"c-09","sid":"s-ed","authority":"user","payload":{"tool":"revert","args":{"instance_id":"ed-1","revision":0}}}
{"id":"m-10","ts":0,"type":"request","from":"console","to":"editor:1","corr":"c-10","sid":"s-ed","authority":"user","payload":{"tool":"read","args":{"instance_id":"ed-1"}}}
{"id":"m-11","ts":0,"type":"request","from":"console","to":"editor:1","corr":"c-11","sid":"s-ed","authority":"user","payload":{"tool":"edit","args":{"instance_id":"ed-1","old":"l","new":"L"}}}
{"id":"m-12","ts":0,"type":"request","from":"console","to":"editor:1","corr":"c-12","sid":"s-ed","authority":"user","payload":{"tool":"edit","args":{"instance_id":"ed-1","old":"NOT PRESENT","new":"x"}}}
{"id":"m-13","ts":0,"type":"request","from":"console","to":"editor:1","corr":"c-13","sid":"s-ed","authority":"user","payload":{"tool":"status"}}
{"id":"m-14","ts":0,"type":"request","from":"console","to":"editor:1","corr":"c-14","sid":"s-ed","authority":"user","payload":{"tool":"close","args":{"instance_id":"ed-2"}}}
{"id":"m-15","ts":0,"type":"request","from":"console","to":"editor:1","corr":"c-15","sid":"s-ed","authority":"user","payload":{"tool":"close","args":{"instance_id":"ed-1"}}}
{"id":"m-16","ts":0,"type":"request","from":"console","to":"editor:1","corr":"c-16","sid":"s-ed","authority":"user","payload":{"tool":"open","args":{"path":"main.rs"}}}
{"id":"m-17","ts":0,"type":"request","from":"console","to":"editor:2","corr":"c-17","sid":"s-ed","authority":"user","payload":{"tool":"open","args":{"path":"main.rs"}}}
{"id":"m-18","ts":0,"type":"request","from":"console","to":"editor:1","corr":"c-18","sid":"s-ed","authority":"user","payload":{"tool":"status"}}
{"id":"m-19","ts":0,"type":"request","from":"console","to":"editor:2","corr":"c-19","sid":"s-ed","authority":"user","payload":{"tool":"status"}}

'@ | Set-Content -LiteralPath $inputPath -Encoding utf8

$outPath = Join-Path $work 'out.ndjson'
$errPath = Join-Path $work 'err.log'

Write-Host "== 跑中控（会话 $sid，托管 $project）==" -ForegroundColor Cyan
Start-Process -FilePath $ctrl `
    -ArgumentList @('--sid', $sid, '--data-dir', (Join-Path $work 'data'), '--services', $servicesPath,
                    '--drain-ms', '4000', '--tick-ms', '0', '--debug-barriers') `
    -RedirectStandardInput $inputPath -RedirectStandardOutput $outPath -RedirectStandardError $errPath `
    -NoNewWindow -Wait

$msgs = Get-Content -LiteralPath $outPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object { $_ | ConvertFrom-Json }
function Reply([string]$corr) { $msgs | Where-Object { $_.type -eq 'result' -and $_.corr -eq $corr } | Select-Object -First 1 }
function From([string]$corr) { (Reply $corr).from }

$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [scriptblock]$cond) {
    if (& $cond) { Write-Host "  [pass] $name" -ForegroundColor Green }
    else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:failures.Add($name) }
}

Write-Host '== 判据 ==' -ForegroundColor Cyan

Check '1. open 拿到实例、revision 0、路径落在托管根下' {
    $r = Reply 'c-02'
    $r -and $r.payload.effect -eq 'confirmed' -and $r.payload.instance_id -eq 'ed-1' -and `
        $r.payload.revision -eq 0 -and $r.payload.path -like '*project*main.rs'
}

Check '2. 同一持有者重复 open 是幂等的' {
    $r = Reply 'c-03'
    $r -and $r.payload.instance_id -eq 'ed-1' -and $r.payload.idempotent -eq $true
}

Check '3. 路径围栏挡住 .. 绕出（并说清托管根在哪）' {
    $r = Reply 'c-04'
    $r -and $r.payload.effect -eq 'refused' -and $r.payload.code -eq 'refused' -and `
        $r.payload.why -like '*不在托管路径内*' -and $r.payload.remedy
}

Check '4. 文件不存在又没给 create 时拒绝（不凭空造文件）' {
    $r = Reply 'c-05'
    $r -and $r.payload.code -eq 'invalid' -and $r.payload.remedy -like '*create*'
}

Check '5. 给了 create 就开出来（ed-2）' {
    $r = Reply 'c-06'
    $r -and $r.payload.effect -eq 'confirmed' -and $r.payload.instance_id -eq 'ed-2' -and $r.payload.created -eq $true
}

Check '6. edit 回的是「编辑之前」的版本号，新内容当场可读' {
    $e = Reply 'c-07'
    $r = Reply 'c-08'
    $e -and $e.payload.changed -eq 1 -and $e.payload.revision -eq 0 -and $e.payload.now_revision -eq 1 -and `
        $r.payload.text -like '*let x = 42;*'
}

Check '7. edit 真的落盘了（另一个文件改完不回滚，跑完看磁盘）' {
    # 空文件也能写 —— old 写空串（这条是写 e2e 时才发现的缺口）
    $r = Reply 'c-06b'
    $diskNew = if (Test-Path (Join-Path $project 'new.txt')) { Get-Content -LiteralPath (Join-Path $project 'new.txt') -Raw } else { '' }
    $r -and $r.payload.effect -eq 'confirmed' -and $diskNew -like '*PERMANENT*'
}

Check '8. revert 把磁盘也退回去了（不只是内存）' {
    $r = Reply 'c-09'
    $disk = Get-Content -LiteralPath $filePath -Raw
    $r -and $r.payload.effect -eq 'confirmed' -and $r.payload.revision -eq 0 -and `
        ($disk -like '*let x = 1;*') -and ($disk -notlike '*42*')
}

Check '9. old 命中的处理：不唯一 / 一处都没有，各自的补救不同' {
    $amb = Reply 'c-11'
    $none = Reply 'c-12'
    $amb -and $none -and $amb.payload.code -eq 'invalid' -and $none.payload.code -eq 'invalid' -and `
        $amb.payload.remedy -like '*写长一点*' -and $none.payload.remedy -like '*read*'
}

Check '10. close 释放写权：关掉之后重新 open 拿到的是新实例' {
    $r = Reply 'c-16'
    $r -and $r.payload.effect -eq 'confirmed' -and $r.payload.instance_id -eq 'ed-3'
}

Check '11. status 看得见锁与开着的文档' {
    $r = Reply 'c-13'
    $r -and $r.payload.data.lock_count -eq 2 -and $r.payload.data.open_count -eq 2 -and `
        ($r.payload.data.locks | Where-Object { $_.held_by.agent -eq 'console' }).Count -eq 2
}

Write-Host ''
Write-Host '== 域冲突：责任在应用这一侧 ==' -ForegroundColor Cyan
Check '12. 第二个实例自己降级，并拒绝碰文件' {
    # 之前这里是"已知空洞"：两个编辑器的锁表互不知情，open 竟然成功了。
    # 现在**编辑器应用自己**在启动时拿域锁，撞上就进 degraded 并且不接活。
    $r = Reply 'c-17'
    $pres = $msgs | Where-Object { $_.type -eq 'presence' -and $_.from -eq 'editor:2' -and $_.payload.state -eq 'degraded' }
    $r -and $r.payload.effect -eq 'refused' -and $r.payload.code -eq 'refused' -and `
        $r.payload.why -like '*已经有另一个*服务在管*' -and $r.payload.remedy -and `
        $pres -and $pres.payload.degraded_reason
}

Check '13. 降级不影响别人：editor:1 照常，且它的锁表里没有 ed-1 的残留' {
    $st = Reply 'c-18'
    $d = $st.payload.data
    $st.payload.effect -eq 'confirmed' -and $d.lock_count -eq 1 -and $d.open_count -eq 1
}

Check '14. 降级实例仍然能被 ping/status 问话（不是含糊的失败）' {
    $r = Reply 'c-19'
    $r -and $r.payload.effect -eq 'confirmed' -and $r.payload.degraded -eq $true
}

Check '15. 注册时就说清"我占了什么"' {
    $pres = $msgs | Where-Object { $_.type -eq 'presence' -and $_.payload.manifest } | Select-Object -First 1
    $pres.payload.manifest.owns.domain -eq 'fs' -and $pres.payload.manifest.owns.exclusive_per -eq 'fence'
}

Check '16. 注册是双向的：中控核对声明并留痕（但不仲裁）' {
    # 中控只是把"这个域有两个主"变成一个看得见的事实（审计），
    # 挡不挡是服务自己的事 —— 所以 remedy 里明说"中控不仲裁这个"。
    $c = $msgs | Where-Object { $_.type -eq 'event' -and $_.payload.code -eq 'domain_conflict' }
    $c -and $c.payload.why -like '*editor:1*' -and $c.payload.remedy -like '*中控不仲裁*'
}

# **落盘屏障的第二个边界**（§5.5）：有副作用的调用，**分派之前先落一次盘**。
# 它在防"日志说没做、文件其实改了"——请求进了写缓冲、服务却已经动了手，这时崩了就没法追溯。
# 判据是服务自己声明的 `destructiveHint`，所以**只读的调用不该落**（别每次 read 都 fsync）。
$errText = Get-Content -LiteralPath $errPath -Raw
Check '17. **有副作用的工具：分派前落了盘**（`edit` 声明了 destructiveHint）' {
    $errText -like '*屏障：分派前先落盘*edit*' -and $errText -like '*destructiveHint*'
}
Check '18. **只读的调用不落盘**（`read` 声明了没有副作用，别白 fsync）' {
    $errText -notlike '*屏障：分派前先落盘*read*'
}

Write-Host ''
Write-Host '== stdout（可观察的消息流，截断显示）==' -ForegroundColor Cyan
Get-Content -LiteralPath $outPath | Where-Object { $_.Trim() -ne '' } | ForEach-Object {
    $m = $_ | ConvertFrom-Json
    $code = if ($m.payload.code) { $m.payload.code } else { '' }
    Write-Host ("{0,-5} {1,-8} {2,-9} → {3,-9} {4}" -f $m.corr, $m.type, $m.from, $m.to, $code)
}

Write-Host ''
Write-Host "== 磁盘上的 $filePath ==" -ForegroundColor Cyan
Get-Content -LiteralPath $filePath | ForEach-Object { Write-Host $_ }

Write-Host ''
Write-Host '== stderr ==' -ForegroundColor Cyan
Get-Content -LiteralPath $errPath | ForEach-Object { Write-Host $_ }

Write-Host ''
if ($failures.Count -gt 0) {
    Write-Host "失败 $($failures.Count) 条：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
Write-Host "工作目录留在：$work"
