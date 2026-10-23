#Requires -Version 7
<#
.SYNOPSIS
  docs/11 R0 / R6 测量的无人值守版本：按顺序跑完 S0–S5，结果写到 OutDir，再用 summarize.py 汇总。

.DESCRIPTION
  和 README §2–§3 的手工流程同一套工具（record.ps1 + probe.py、frame_probe、drive_app.mjs），区别是：
  - 不需要人在场。"使用中"档用零位移的 SendInput 模拟（光标不动），每 5 秒一次；同时监视真实输入，
    测量期间有人用电脑时在 interference.csv 里记下来。
  - 改动前后对比用治理器的覆盖模式：F = "全速"（缩略图 6 并发、AI 不限 GPU，近似改动前不限速），
    A = "自动 + 有人在用"（使用中档）。
  - 缩略图负载靠重置 test 库里一批图的缩略图状态、再重启应用让它重排（应用关闭时改库，只动 test profile）。
    NAS 原图第二次读取时可能命中系统文件缓存，网络读取量会偏低，CPU 解码编码量不变。
  - frame_probe 用独立的浏览器实例（frame_probe_run.mjs），窗口和应用窗口在各自测量的那一分钟置顶。

  场景（每个 DurationSec 秒，默认 180）：
    S0  ×2  应用不开（基线）
    S1      应用打开，没有后台任务
    S2F     扫描 NAS 目录 + 生成缩略图，全速
    S2A     重排一批缩略图，使用中（结束后测可见区域出图耗时）
    S3F     只跑 AI（图像向量 + 标签），全速；含 worker 冷启动
    S3A     只跑 AI，使用中（GPU 闸门暂停）
    S4F     缩略图 + AI，全速
    S4A     缩略图 + AI，使用中
    S5      降档速度：空闲档全速运行时模拟一次输入，看 CPU / GPU 多久降下来（需要电脑连续 3 分钟以上没人碰）

.EXAMPLE
  pwsh -NoProfile -File scripts\perf\run_scenarios.ps1 -OutDir D:\perf\r6-20261023
#>
param(
    [string]$OutDir = "D:\perf\r6-$(Get-Date -Format yyyyMMdd)",
    [string]$Exe = (Join-Path $PSScriptRoot '..\..\src-tauri\target\release\photo-view-plus.exe'),
    [string]$NasRoot = 'Y:\重要文件夹\唐唐手机备份\PD2324\PD2324相册备份\Camera',
    [int]$DurationSec = 180,
    [int]$ResetCount = 2500,
    # test 库里这个根目录的标签：第一次运行时按 NasRoot 新建，之后按标签找回来。
    [string]$RootLabel = 'perf-camera',
    [string]$PythonExe = 'python',
    [string[]]$Only = @()
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
# pwsh -File 传进来的是一个 "S0,S1" 字符串，按逗号拆开。
$Only = @($Only | ForEach-Object { $_ -split ',' } | Where-Object { $_ })

# 各时间点按场景时长缩放（180 秒时同 README §2：第 20 秒 frame_probe、第 95 秒滚动画廊，各 60 秒）。
$FrameAt = [int]($DurationSec * 20 / 180)
$GalleryAt = [int]($DurationSec * 95 / 180)
$WindowSec = [Math]::Max(10, [int]($DurationSec / 3))
$Cooldown = [Math]::Min(30, [int]($DurationSec / 6))
$Drive = Join-Path $PSScriptRoot 'drive_app.mjs'
$DataDir = Join-Path $env:LOCALAPPDATA 'com.vetoer.photoviewplus.test'
$DbPath = Join-Path $DataDir 'db\app.sqlite'
$ProbeProfile = Join-Path ([System.IO.Path]::GetTempPath()) 'pvp-frame-probe-profile'
New-Item -ItemType Directory -Force $OutDir | Out-Null
$EventLog = Join-Path $OutDir 'events.log'
$Interference = Join-Path $OutDir 'interference.csv'
if (-not (Test-Path $Interference)) { 'label,start,end,user_inputs,nudges' | Set-Content $Interference -Encoding utf8 }

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class PerfWin {
    [StructLayout(LayoutKind.Sequential)] struct LASTINPUTINFO { public uint cbSize; public uint dwTime; }
    [DllImport("user32.dll")] static extern bool GetLastInputInfo(ref LASTINPUTINFO plii);
    [StructLayout(LayoutKind.Sequential)] struct MOUSEINPUT { public int dx; public int dy; public uint mouseData; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
    [StructLayout(LayoutKind.Sequential)] struct INPUT { public uint type; public MOUSEINPUT mi; }
    [DllImport("user32.dll", SetLastError = true)] static extern uint SendInput(uint nInputs, INPUT[] pInputs, int cbSize);
    [DllImport("user32.dll")] static extern bool SetWindowPos(IntPtr hWnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
    public static double IdleSeconds() {
        var info = new LASTINPUTINFO(); info.cbSize = (uint)Marshal.SizeOf(info);
        GetLastInputInfo(ref info);
        return unchecked((uint)Environment.TickCount - info.dwTime) / 1000.0;
    }
    public static bool Nudge() {
        var input = new INPUT[1];
        input[0].type = 0;            // INPUT_MOUSE
        input[0].mi.dwFlags = 0x0001; // MOUSEEVENTF_MOVE，dx = dy = 0：光标不动，只刷新"最近输入时间"
        return SendInput(1, input, Marshal.SizeOf(typeof(INPUT))) == 1;
    }
    public static bool TopMost(IntPtr hWnd, bool on) {
        // HWND_TOPMOST = -1 / HWND_NOTOPMOST = -2；SWP_NOSIZE | SWP_NOMOVE | SWP_SHOWWINDOW
        return SetWindowPos(hWnd, new IntPtr(on ? -1 : -2), 0, 0, 0, 0, 0x0001 | 0x0002 | 0x0040);
    }
}
'@

function Log([string]$message) {
    $line = '{0}  {1}' -f (Get-Date -Format 'yyyy-MM-dd HH:mm:ss.fff'), $message
    Write-Host $line
    Add-Content -Path $EventLog -Value $line -Encoding utf8
}

function Want([string]$label) { $Only.Count -eq 0 -or $Only -contains $label }

function Invoke-App([string]$command, [string[]]$arguments = @()) {
    $raw = & node $Drive invoke $command @arguments 2>&1
    if ($LASTEXITCODE -ne 0) { throw "drive_app $command 失败：$raw" }
    $text = ($raw | Out-String).Trim()
    if (-not $text -or $text -eq 'null') { return $null }
    return $text | ConvertFrom-Json
}

function Test-AppUp {
    try { return (& node $Drive invoke ping 2>$null | Out-String).Trim() -eq '"test"' } catch { return $false }
}

$script:AppProc = $null

function Start-App([string]$override = 'auto') {
    if (Get-Process photo-view-plus -ErrorAction SilentlyContinue) { throw '已有 photo-view-plus 在运行，先关掉' }
    $env:PVP_PROFILE = 'test'
    $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = '--remote-debugging-port=9222'
    if ($override -eq 'auto') { [PerfWin]::Nudge() | Out-Null }
    $script:AppProc = Start-Process -FilePath $Exe -PassThru
    Remove-Item Env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS
    $deadline = (Get-Date).AddSeconds(90)
    while (-not (Test-AppUp)) {
        # 构建和 PVP_PROFILE 对不上时应用会拒绝启动、立刻退出（README §0.2），不用等满 90 秒。
        if ($script:AppProc.HasExited) { $script:AppProc = $null; throw '应用启动后立即退出：多半是 exe 不是 test profile 的构建（见 test 日志里的 ERROR，按 README §0.2 重新构建）' }
        if ((Get-Date) -gt $deadline) { throw '应用 90 秒内没有就绪' }
        Start-Sleep -Milliseconds 500
    }
    $log = Get-ChildItem (Join-Path $DataDir 'logs') -Filter 'photo-view-plus.log*' | Sort-Object LastWriteTime | Select-Object -Last 1
    $line = (Select-String -Path $log.FullName -Pattern 'opening database' | Select-Object -Last 1).Line
    if ($line -notmatch 'com\.vetoer\.photoviewplus\.test\\+db') { Stop-App; throw "打开的不是 test 库：$line" }
    Set-Override $override
    Log "app started pid=$($script:AppProc.Id) override=$override"
}

function Stop-App {
    Get-Process photo-view-plus -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Seconds 3
    $script:AppProc = $null
    Log 'app stopped'
}

function Set-Override([string]$mode) { Invoke-App governor_set_override @("mode=$mode") | Out-Null }

function Set-Ai([bool]$enabled) {
    Invoke-App settings_update @("patch.aiEnabled=$($enabled.ToString().ToLower())") | Out-Null
    Log "aiEnabled=$enabled"
}

function Get-Queue { Invoke-App queue_status }

# 等扫描和缩略图全部做完：排队的（p0 / p1）和正在跑的都要清零。扫描任务运行时不在排队数里，只看排队数会提前返回。
function Wait-Drained([int]$timeoutSec = 900) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    do {
        Start-Sleep -Seconds 10
        $q = Get-Queue
        Log ("drain p0={0} p1={1} running={2}" -f $q.p0, $q.p1, $q.running)
    } while (($q.p0 + $q.p1 + $q.running) -gt 0 -and (Get-Date) -lt $deadline)
}

$PerfDb = Join-Path $PSScriptRoot 'perf_db.py'

function Invoke-PerfDb([string[]]$arguments) {
    $out = & $PythonExe $PerfDb @arguments
    if ($LASTEXITCODE -ne 0) { throw "perf_db.py $($arguments -join ' ') 失败：$out" }
    return ($out | Out-String).Trim()
}

function Get-RootId {
    $out = Invoke-PerfDb @('root-id', $DbPath, $RootLabel, $NasRoot)
    if ($out) { return [int]$out } else { return $null }
}

# 应用关闭时把一批已就绪的缩略图标回待生成，重启后由启动重排交给批量队列（只动 test 库）。
function Reset-Thumbnails([string]$selection, [int]$limit) {
    if ($script:AppProc) { throw '改库前要先关应用' }
    $root = Get-RootId
    if (-not $root) { throw "test 库里没有根目录 $NasRoot" }
    $changed = Invoke-PerfDb @('reset-thumbs', $DbPath, $root, $selection, $limit)
    Log "reset thumbnails selection=$selection limit=$limit changed=$changed"
    return [int]$changed
}

# 把一批已算过图像向量的图标回待处理，给 AI 场景造负载（应用运行时也可以做）。
function Reset-Ai([int]$limit) {
    $changed = Invoke-PerfDb @('reset-ai', $DbPath, $limit)
    Log "reset ai limit=$limit changed=$changed"
    return [int]$changed
}

# 后台线程：mode = nudge 时每 5 秒模拟一次输入（保持"使用中"），并统计不是自己造成的输入（有人在用电脑）。
function Start-Activity([string]$mode, [int]$seconds) {
    Start-ThreadJob -ArgumentList $mode, $seconds -ScriptBlock {
        param($mode, $seconds)
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        $lastNudge = -10.0
        $expect = $false
        $previous = [PerfWin]::IdleSeconds()
        $user = 0
        $nudges = 0
        while ($sw.Elapsed.TotalSeconds -lt $seconds) {
            $now = $sw.Elapsed.TotalSeconds
            if ($mode -eq 'nudge' -and $now - $lastNudge -ge 5) {
                if ([PerfWin]::Nudge()) { $nudges++; $expect = $true }
                $lastNudge = $now
            }
            Start-Sleep -Milliseconds 250
            $idle = [PerfWin]::IdleSeconds()
            if ($idle + 0.1 -lt $previous) {
                if ($expect) { $expect = $false } else { $user++ }
            }
            $previous = $idle
        }
        [pscustomobject]@{ user = $user; nudges = $nudges }
    }
}

function Set-WindowTopMost([System.Diagnostics.Process]$process, [bool]$on) {
    if (-not $process) { return }
    $process.Refresh()
    if ($process.MainWindowHandle -ne [IntPtr]::Zero) { [PerfWin]::TopMost($process.MainWindowHandle, $on) | Out-Null }
}

function Invoke-FrameProbe([string]$label) {
    $runner = Join-Path $PSScriptRoot 'frame_probe_run.mjs'
    $proc = Start-Process node -ArgumentList @("`"$runner`"", '--out', "`"$OutDir`"", '--label', $label, '--duration', $WindowSec, '--profile', "`"$ProbeProfile`"") -PassThru -WindowStyle Hidden
    # 等浏览器窗口出现后置顶，测完关掉
    $deadline = (Get-Date).AddSeconds(30)
    $window = $null
    while (-not $window -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 500
        $window = Get-Process chrome, msedge -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowTitle -eq 'Frame Probe' } | Select-Object -First 1
    }
    if ($window) { [PerfWin]::TopMost($window.MainWindowHandle, $true) | Out-Null }
    if (-not $proc.WaitForExit(120000)) { $proc.Kill(); Log "frame probe $label timed out" }
    Log "frame probe $label exit=$($proc.ExitCode)"
}

function Invoke-GalleryScroll([string]$label) {
    Set-WindowTopMost $script:AppProc $true
    $proc = Start-Process node -ArgumentList @("`"$Drive`"", '--out', "`"$OutDir`"", '--label', $label, 'scroll-gallery', $WindowSec) -PassThru -WindowStyle Hidden
    if (-not $proc.WaitForExit(120000)) { $proc.Kill(); Log "gallery scroll $label timed out" }
    Set-WindowTopMost $script:AppProc $false
    Log "gallery scroll $label exit=$($proc.ExitCode)"
}

function Wait-Until([datetime]$start, [double]$offsetSec) {
    $target = $start.AddSeconds($offsetSec)
    $remain = ($target - (Get-Date)).TotalMilliseconds
    if ($remain -gt 0) { Start-Sleep -Milliseconds ([int]$remain) }
}

# 一个场景：record + probe 全程；第 5 秒触发负载；第 20 秒 frame_probe；第 95 秒滚动画廊（应用在运行时）。
function Invoke-Scenario([string]$label, [scriptblock]$trigger, [switch]$App, [switch]$Nudge) {
    Log "=== $label start"
    $start = Get-Date
    $record = Start-Process pwsh -ArgumentList @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$(Join-Path $PSScriptRoot 'record.ps1')`"",
        '-DurationSec', $DurationSec, '-OutDir', "`"$OutDir`"", '-Label', $label, '-WithProbe', '-PythonExe', $PythonExe) -PassThru -WindowStyle Hidden
    $activity = Start-Activity ($(if ($Nudge) { 'nudge' } else { 'watch' })) ($DurationSec + 5)
    Wait-Until $start 5
    if ($trigger) { & $trigger }
    Wait-Until $start $FrameAt
    Invoke-FrameProbe $label
    if ($App) {
        Wait-Until $start $GalleryAt
        Invoke-GalleryScroll $label
    }
    $record.WaitForExit()
    $result = Receive-Job $activity -Wait -AutoRemoveJob
    '{0},{1},{2},{3},{4}' -f $label, $start.ToString('o'), (Get-Date).ToString('o'), $result.user, $result.nudges | Add-Content $Interference -Encoding utf8
    Log ("=== $label done (user inputs during run: {0}, nudges: {1})" -f $result.user, $result.nudges)
}

function Measure-VisibleLatency {
    $script = @'
(async () => {
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  // 只看这个根目录：在侧栏里点它（按标签匹配）。
  const rootItem = [...document.querySelectorAll(".app-sidebar button")].find((b) => b.innerText.includes("__ROOT_LABEL__"));
  if (rootItem) { rootItem.click(); await sleep(2500); }
  const el = document.querySelector(".virtual-image-grid");
  for (let i = 0; i < 5; i++) {
    const more = document.querySelector(".load-more");
    if (!more) break;
    more.click();
    await sleep(1500);
  }
  const out = [];
  for (const screens of [10, 20, 30]) {
    el.scrollTop = el.clientHeight * screens;
    await sleep(60);
    el.scrollTop += 2;
    const start = performance.now();
    let pendingAtStart = null;
    let done = false;
    while (performance.now() - start < 10000) {
      const box = el.getBoundingClientRect();
      const visible = [...document.querySelectorAll(".image-card")].filter((c) => {
        const r = c.getBoundingClientRect();
        return r.bottom > box.top && r.top < box.bottom;
      });
      const pending = visible.filter((c) => !c.querySelector("img")).length;
      if (pendingAtStart === null) pendingAtStart = pending;
      if (pending === 0) { done = true; break; }
      await sleep(100);
    }
    out.push({ screens, pendingAtStart, ms: Math.round(performance.now() - start), done });
    await sleep(1500);
  }
  return JSON.stringify(out);
})()
'@
    $script = $script.Replace('__ROOT_LABEL__', $RootLabel)
    Set-WindowTopMost $script:AppProc $true
    $raw = & node $Drive eval $script 2>&1
    Set-WindowTopMost $script:AppProc $false
    $text = ($raw | Out-String).Trim()
    Set-Content -Path (Join-Path $OutDir 'visible_latency.json') -Value $text -Encoding utf8
    Log "visible latency: $text"
}

# S5：空闲档全速运行时模拟一次输入，记录档位切换和负载回落的时间。
function Invoke-Downgrade {
    Log '=== S5: waiting for idle tier (needs 3+ minutes without real input)'
    $deadline = (Get-Date).AddMinutes(10)
    do {
        Start-Sleep -Seconds 5
        $status = Invoke-App governor_status
    } while ($status.tier -ne 'idle' -and (Get-Date) -lt $deadline)
    if ($status.tier -ne 'idle') { Log '=== S5 skipped: never reached idle tier (someone is using the computer)'; return }
    Log 'idle tier reached; letting full-speed load settle for 20s'
    Start-Sleep -Seconds 20
    $start = Get-Date
    $record = Start-Process pwsh -ArgumentList @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$(Join-Path $PSScriptRoot 'record.ps1')`"",
        '-DurationSec', 90, '-OutDir', "`"$OutDir`"", '-Label', 'S5', '-WithProbe', '-PythonExe', $PythonExe) -PassThru -WindowStyle Hidden
    Wait-Until $start 30
    [PerfWin]::Nudge() | Out-Null
    $nudgedAt = Get-Date
    $switchedAt = $null
    $lastNudge = $nudgedAt
    while (((Get-Date) - $start).TotalSeconds -lt 88) {
        if (-not $switchedAt) {
            $status = Invoke-App governor_status
            if ($status.tier -eq 'active') { $switchedAt = Get-Date }
        }
        if (((Get-Date) - $lastNudge).TotalSeconds -ge 5) { [PerfWin]::Nudge() | Out-Null; $lastNudge = Get-Date }
        Start-Sleep -Milliseconds 200
    }
    $record.WaitForExit()
    $events = [pscustomobject]@{
        recordStart = $start.ToString('o')
        nudge = $nudgedAt.ToString('o')
        tierActive = if ($switchedAt) { $switchedAt.ToString('o') } else { $null }
        switchMs = if ($switchedAt) { [int]($switchedAt - $nudgedAt).TotalMilliseconds } else { $null }
    }
    $events | ConvertTo-Json | Set-Content (Join-Path $OutDir 'S5_events.json') -Encoding utf8
    Log "=== S5 done: $($events | ConvertTo-Json -Compress)"
}

# ------------------------------------------------------------------ 主流程

if (-not (Test-Path $Exe)) { throw "找不到 $Exe（先按 README §0.2 构建 test profile 的 release exe）" }
if (-not (Test-Path $NasRoot)) { throw "访问不到 $NasRoot" }
Log "run_scenarios start: out=$OutDir exe=$Exe root=$NasRoot duration=$DurationSec"

try {
    if (Want 'S0') {
        if (Get-Process photo-view-plus -ErrorAction SilentlyContinue) { throw 'S0 需要应用没在运行' }
        foreach ($round in 1..2) { Invoke-Scenario 'S0'; Start-Sleep -Seconds $Cooldown }
    }

    Start-App 'auto'
    Set-Ai $false

    if (Want 'S1') { Invoke-Scenario 'S1' -App; Start-Sleep -Seconds $Cooldown }

    if (Want 'S2F') {
        Set-Override 'fullSpeed'
        Invoke-Scenario 'S2F' -App -Trigger {
            $root = @(Invoke-App roots_list) | Where-Object { $_.label -eq $RootLabel -or $_.path -eq $NasRoot } | Select-Object -First 1
            if (-not $root) { $root = Invoke-App roots_add @("args.path=$NasRoot", "args.label=$RootLabel") }
            Invoke-App scan_start @("rootId=$($root.id)") | Out-Null
            Log "scan started root=$($root.id)"
        }
        Log 'draining scan + thumbnails at full speed'
        Wait-Drained 1200
    }

    if (Want 'S2A') {
        Stop-App
        Reset-Thumbnails 'newest' $ResetCount | Out-Null
        Start-App 'auto'
        Invoke-Scenario 'S2A' -App -Nudge
        $keep = Start-Activity 'nudge' 60
        Measure-VisibleLatency
        Receive-Job $keep -Wait -AutoRemoveJob | Out-Null
        Set-Override 'fullSpeed'
        Wait-Drained 900
    }

    if (Want 'S3F') {
        Set-Override 'fullSpeed'
        Invoke-Scenario 'S3F' -App -Trigger { Set-Ai $true; Invoke-App ai_process_pending | Out-Null }
    }

    if (Want 'S3A') {
        Set-Ai $true
        Set-Override 'auto'
        $warm = Start-Activity 'nudge' 20
        Reset-Ai 3000 | Out-Null
        Receive-Job $warm -Wait -AutoRemoveJob | Out-Null
        Invoke-Scenario 'S3A' -App -Nudge
    }

    if (Want 'S4F') {
        Stop-App
        Reset-Thumbnails 'oldest' 4000 | Out-Null
        Reset-Ai 3000 | Out-Null
        Start-App 'fullSpeed'
        Set-Ai $true
        Log 'S4F: waiting 45s for the AI worker to warm up'
        Start-Sleep -Seconds 45
        Invoke-Scenario 'S4F' -App
    }

    if (Want 'S4A') {
        Stop-App
        Reset-Thumbnails 'oldest' $ResetCount | Out-Null
        Reset-Ai 3000 | Out-Null
        Start-App 'auto'
        Set-Ai $true
        $warm = Start-Activity 'nudge' 45
        Start-Sleep -Seconds 45
        Receive-Job $warm -Wait -AutoRemoveJob | Out-Null
        Invoke-Scenario 'S4A' -App -Nudge
    }

    if (Want 'S5') {
        Stop-App
        Reset-Thumbnails 'oldest' 4000 | Out-Null
        Reset-Ai 3000 | Out-Null
        Start-App 'auto'
        Set-Ai $true
        Invoke-Downgrade
    }
}
finally {
    if (Get-Process photo-view-plus -ErrorAction SilentlyContinue) { Stop-App }
    Log 'run_scenarios finished'
}

& $PythonExe (Join-Path $PSScriptRoot 'summarize.py') $OutDir --all | Out-Null
Log "summary: $(Join-Path $OutDir 'summary.md')"
