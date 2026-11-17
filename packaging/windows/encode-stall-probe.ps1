#requires -Version 7
<#
.SYNOPSIS
  Gate G3 fault injection on the test box: deploy the encode-probe pf-vdisplay, stream a loopback
  session, wedge the driver's encode thread with an unbounded wait, and measure what it cost.

.DESCRIPTION
  The driver's encode thread parks forever once it has encoded PFVD_ENCODE_BLOCK_AFTER frames
  (probe builds only, packaging/windows/drivers/pf-vdisplay/src/encode/thread.rs). The G3 claim is
  that this costs one IDR and never a compose hitch, so the run measures three things:

    * COMPOSE - `capture.dropped_total` + `capture.published_total` from `punktfunk-host ctl
      --json status`, polled twice a second. Every drain-worker pass through the block window
      bumps one of them (the pool has no free slot, so the frame is counted and dropped), so their
      combined rate IS the drain worker's cadence. A hitch shows as an interval below refresh.
    * LADDER - the rungs the host ran, from `capture.last_episode.stages[]` (the coordinator's own
      per-rung ms) and from the host.log timestamps of the lines each actuator writes.
    * AU STREAM - the host's access-unit dump (PUNKTFUNK_IDD_DIAG, set as a per-service
      environment value so a service restart picks it up without a reboot): per-AU `qpc_pts`
      deltas and keyframe count, i.e. how many IDRs the recovery really cost.

  The knob is disarmed as soon as the wedge is seen, so the first encoder reset succeeds and the
  run measures one IDR. `-WalkLadder` leaves it armed instead: every reopen re-wedges and the
  ladder escalates EncoderReset -> SwapChainReset -> PresentationReset -> DriverCycle.

  Run ELEVATED, on the box, from a detached console-user task (kick-task.ps1). Arguments carry no
  commas and no colons.

.EXAMPLE
  pwsh -NoProfile -File encode-stall-probe.ps1 -Ref wp/vp-p4-split -Tag g3
.EXAMPLE
  pwsh -NoProfile -File encode-stall-probe.ps1 -Tag g3walk -SkipBuild -WalkLadder -WatchSecs 240
#>
[CmdletBinding()]
param(
    [string]$Ref = 'wp/vp-p4-split',
    [string]$Tag = 'g3',
    # Frames encoded before the thread parks. 600 is ~10 s at 60 Hz: the session streams healthy first.
    [int]$BlockAfter = 600,
    # Seconds of streaming to require before the wedge counts as "came up healthy".
    [int]$HealthySecs = 25,
    # Seconds to keep watching after the wedge.
    [int]$WatchSecs = 150,
    # Leave the knob armed so every encoder reopen re-wedges and the whole ladder runs.
    [switch]$WalkLadder,
    [switch]$SkipBuild,
    [switch]$SkipDeploy,
    [string]$Repo = 'C:\Users\Public\pf-phase0',
    [string]$Client = 'C:\Users\Public\punktfunk-native\target\release\punktfunk-session.exe',
    [string]$HostExe = 'C:\Program Files\punktfunk\punktfunk-host.exe',
    [string]$Service = 'PunktfunkHost',
    [string]$OutDir = 'C:\Users\Public'
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$runLog = Join-Path $OutDir "stall-$Tag.log"
$pollLog = Join-Path $OutDir "stall-$Tag-poll.jsonl"
$clientLog = Join-Path $OutDir "stall-$Tag-client.out"
$dumpDir = Join-Path $OutDir "stall-$Tag-au"
$hostLog = Join-Path $env:ProgramData 'punktfunk\logs\host.log'
$machineEnv = 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Environment'
$svcEnv = "HKLM:\SYSTEM\CurrentControlSet\Services\$Service"

function Say([string]$m) {
    $line = '{0}  {1}' -f (Get-Date -Format 'HH:mm:ss.fff'), $m
    Write-Host $line
    Add-Content -Path $runLog -Value $line
}

# The knob lives in the MACHINE environment because WUDFHost's own is stale until a reboot; the
# driver's `knob()` reads this key live, so a value written here reaches the next encoder open.
function Set-BlockKnob([int]$n) {
    if ($n -gt 0) { Set-ItemProperty -Path $machineEnv -Name 'PFVD_ENCODE_BLOCK_AFTER' -Value "$n" }
    else { Remove-ItemProperty -Path $machineEnv -Name 'PFVD_ENCODE_BLOCK_AFTER' -ErrorAction SilentlyContinue }
}

function Get-Capture {
    $raw = & $HostExe ctl --json status 2>$null
    if (-not $raw) { return $null }
    try { ($raw | ConvertFrom-Json).capture } catch { $null }
}

Remove-Item $runLog, $pollLog, $clientLog -Force -ErrorAction SilentlyContinue
Remove-Item $dumpDir -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $dumpDir | Out-Null
Say "run $Tag  ref=$Ref  block-after=$BlockAfter  walk-ladder=$($WalkLadder.IsPresent)"

# --- 1. build the probe driver -----------------------------------------------------------------
$drivers = Join-Path $Repo 'packaging\windows\drivers'
if (-not $SkipBuild) {
    Say "fetch $Ref into $Repo"
    Push-Location $Repo
    try {
        & git fetch origin $Ref
        if ($LASTEXITCODE -ne 0) { throw "git fetch $Ref failed" }
        & git checkout -B pf-stall-probe FETCH_HEAD
        if ($LASTEXITCODE -ne 0) { throw 'git checkout failed' }
    }
    finally { Pop-Location }
    if (-not $env:Version_Number) { $env:Version_Number = '10.0.26100.0' }
    if (-not $env:LIBCLANG_PATH -and (Test-Path 'C:\Program Files\LLVM\bin\libclang.dll')) {
        $env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin'
    }
    # wdk-sys walks up from OUT_DIR for a Cargo.lock, so the driver must use its own target dir.
    Remove-Item Env:\CARGO_TARGET_DIR -ErrorAction SilentlyContinue
    Say 'cargo build --release -p pf-vdisplay --features encode-probe'
    Push-Location $drivers
    try {
        & cargo build --release -p pf-vdisplay --features encode-probe
        if ($LASTEXITCODE -ne 0) { throw "driver build failed ($LASTEXITCODE)" }
    }
    finally { Pop-Location }
}

# --- 2. deploy + prove a FRESH WUDFHost picked the new image up ---------------------------------
if (-not $SkipDeploy) {
    $deployedAt = Get-Date
    Say 'redeploy-pf-vdisplay.ps1'
    & (Join-Path $Repo 'packaging\windows\redeploy-pf-vdisplay.ps1') -Service $Service
    # A redeploy replaces the DLL under a WUDFHost that keeps the OLD mapped image, and reports
    # success either way: only a host started after the deploy is running the probe build.
    $wudf = Get-Process WUDFHost -ErrorAction SilentlyContinue |
        Where-Object { $_.Modules.ModuleName -contains 'pf_vdisplay.dll' }
    if ($wudf -and ($wudf | Where-Object { $_.StartTime -lt $deployedAt })) {
        Say 'stale WUDFHost still holds pf_vdisplay.dll - forcing a fresh one'
        Stop-Service $Service -Force -ErrorAction SilentlyContinue
        $wudf | Stop-Process -Force -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 2
        & (Join-Path $Repo 'packaging\windows\reset-pf-vdisplay.ps1') -NoHost
        Start-Service $Service
        Start-Sleep -Seconds 3
    }
}

# --- 3. AU dump + knob --------------------------------------------------------------------------
# Per-service Environment (REG_MULTI_SZ): the SCM applies it at service start, so unlike `setx /M`
# this reaches the host without a reboot.
Set-ItemProperty -Path $svcEnv -Name 'Environment' -Type MultiString -Value @("PUNKTFUNK_IDD_DIAG=$dumpDir")
Restart-Service $Service -Force
Start-Sleep -Seconds 3
Set-BlockKnob $BlockAfter
Say "armed PFVD_ENCODE_BLOCK_AFTER=$BlockAfter  au-dump=$dumpDir"
$hostMark = (Get-Item $hostLog -ErrorAction SilentlyContinue).Length
if (-not $hostMark) { $hostMark = 0 }

# --- 4. loopback client -------------------------------------------------------------------------
$cert = Join-Path $env:ProgramData 'punktfunk\cert.pem'
$fp = (Get-FileHash -Algorithm SHA256 $cert).Hash.ToLower()
Say "starting the loopback client (fp $fp)"
$proc = Start-Process -FilePath $Client -PassThru -WindowStyle Minimized `
    -ArgumentList @('--connect', '127.0.0.1:9777', '--fp', $fp) `
    -RedirectStandardOutput $clientLog -RedirectStandardError "$clientLog.err"

# --- 5. poll -----------------------------------------------------------------------------------
$t0 = Get-Date
$blockedAt = $null
$lastPub = -1
$stopAt = $t0.AddSeconds($HealthySecs + $WatchSecs + 30)
try {
    while ((Get-Date) -lt $stopAt) {
        $c = Get-Capture
        $now = Get-Date
        if ($c) {
            $row = [ordered]@{
                t = $now.ToString('o'); class = $c.class; stall = $c.stall_class
                gap_ms = $c.source_gap_ms; state = $c.encoder_state; stage = $c.current_stage
                detached = $c.detached; pub = $c.published_total; drop = $c.dropped_total
            }
            Add-Content -Path $pollLog -Value ($row | ConvertTo-Json -Compress)
            # The wedge: past the armed frame count, publishing stopped. An idle desktop also
            # stops publishing, hence the frame-count floor - before it the thread cannot be parked.
            if (-not $blockedAt -and $c.published_total -ge $BlockAfter -and $c.published_total -eq $lastPub `
                    -and ($now - $t0).TotalSeconds -gt $HealthySecs) {
                $blockedAt = $now
                Say "wedge at published_total=$($c.published_total) after $([int]($now - $t0).TotalSeconds) s"
                if (-not $WalkLadder) { Set-BlockKnob 0; Say 'knob disarmed - the next reset may hold' }
            }
            $lastPub = $c.published_total
        }
        if ($blockedAt -and ($now - $blockedAt).TotalSeconds -gt $WatchSecs) { break }
        if ($proc.HasExited) { Say "client exited early (code $($proc.ExitCode))"; break }
        Start-Sleep -Milliseconds 500
    }
}
finally {
    Set-BlockKnob 0
    if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
}
if (-not $blockedAt) { Say 'NO WEDGE OBSERVED - the knob never fired (probe build deployed?)' }

# --- 6. compose cadence across the block window -------------------------------------------------
Say '--- compose cadence (drain-worker passes per second, from published+dropped) ---'
$rows = @(Get-Content $pollLog -ErrorAction SilentlyContinue | ForEach-Object { $_ | ConvertFrom-Json })
$rates = @()
for ($i = 1; $i -lt $rows.Count; $i++) {
    $dt = ([datetime]$rows[$i].t - [datetime]$rows[$i - 1].t).TotalSeconds
    if ($dt -le 0) { continue }
    $d = ($rows[$i].pub + $rows[$i].drop) - ($rows[$i - 1].pub + $rows[$i - 1].drop)
    $rates += [pscustomobject]@{ t = [datetime]$rows[$i].t; fps = [math]::Round($d / $dt, 1) }
}
if ($blockedAt) {
    $win = $rates | Where-Object { $_.t -ge $blockedAt.AddSeconds(-5) -and $_.t -le $blockedAt.AddSeconds(30) }
    if ($win) {
        $f = $win.fps | Sort-Object
        Say ("block window n={0}  min={1}  median={2}  max={3} fps" -f $f.Count, $f[0], $f[[int]($f.Count / 2)], $f[-1])
        Say ('intervals below 45 fps: ' + (($win | Where-Object { $_.fps -lt 45 }).Count))
    }
}
$all = @($rates.fps) | Sort-Object
if ($all) { Say ("whole run n={0}  min={1}  median={2}" -f $all.Count, $all[0], $all[[int]($all.Count / 2)]) }

# --- 7. the ladder: per-rung wall clock ---------------------------------------------------------
Say '--- recovery ladder ---'
$marks = @(
    'capture health changed', 'IDD push: recovery stage', 'recovery: encoder reset applied',
    'forced same-mode reset applied', 'IDD push: same-mode presentation restart',
    'recovery: driver cycle', 'IDD push: recovery episode closed', 'IDD push: recovery ladder exhausted',
    'same-mode recovery refused', 'IDD push: the pf-vdisplay WUDFHost is gone'
)
$rx = ($marks | ForEach-Object { [regex]::Escape($_) }) -join '|'
$fs = [IO.File]::Open($hostLog, 'Open', 'Read', 'ReadWrite')
$fs.Seek($hostMark, 'Begin') | Out-Null
$tail = (New-Object IO.StreamReader($fs)).ReadToEnd()
$fs.Dispose()
$hits = $tail -split "`n" | ForEach-Object { $_ -replace "`e\[[0-9;]*m", '' } | Where-Object { $_ -match $rx }
$hits | ForEach-Object { Say "  $_" }
# Rung wall clock: each actuator's own line to the next one. `IDD push: recovery stage` is written
# AFTER the actuator returned, so consecutive stage lines bracket the rung plus its proof wait.
$stamped = $hits | ForEach-Object {
    if ($_ -match '^(?<ts>[0-9T:\.\-]{19,})') { [pscustomobject]@{ t = [datetime]$Matches.ts; line = $_ } }
} | Where-Object { $_ }
for ($i = 1; $i -lt $stamped.Count; $i++) {
    $ms = [int]($stamped[$i].t - $stamped[$i - 1].t).TotalMilliseconds
    Say ("  +{0} ms -> {1}" -f $ms, ($stamped[$i].line -replace '\s+', ' ').Substring(0, [Math]::Min(140, $stamped[$i].line.Length)))
}
$final = Get-Capture
if ($final -and $final.last_episode) {
    Say ("episode {0} recovered={1} took={2} ms" -f $final.last_episode.stall_class, $final.last_episode.recovered, $final.last_episode.took_ms)
    $final.last_episode.stages | ForEach-Object { Say ("  rung {0} {1} {2} ms" -f $_.stage, $_.outcome, $_.took_ms) }
}
if ($final) { Say ("final class={0} state={1} detached={2} published={3}" -f $final.class, $final.encoder_state, $final.detached, $final.published_total) }

# --- 8. AU stream: qpc_pts deltas + IDR count ---------------------------------------------------
Say '--- access units (qpc_pts deltas, keyframes) ---'
$dump = Get-ChildItem $dumpDir -Filter 'pfvd-au-*.bin' -ErrorAction SilentlyContinue |
    Sort-Object Length -Descending | Select-Object -First 1
if (-not $dump) { Say '  no AU dump (PUNKTFUNK_IDD_DIAG did not reach the service)' }
else {
    # Framing: [u32 len][u64 pts_ns][u8 keyframe][len bytes]. Seek past the payload - the file is
    # the whole encoded stream.
    $s = [IO.File]::OpenRead($dump.FullName)
    $r = New-Object IO.BinaryReader($s)
    $prev = 0L; $n = 0; $kf = 0; $gaps = @(); $worst = 0L
    while ($s.Position -lt $s.Length - 13) {
        $len = $r.ReadUInt32(); $pts = $r.ReadUInt64(); $key = $r.ReadByte()
        if ($s.Position + $len -gt $s.Length) { break }
        $s.Seek($len, 'Current') | Out-Null
        $n++; if ($key -ne 0) { $kf++ }
        if ($prev -gt 0 -and $pts -gt $prev) {
            $d = [int64](($pts - $prev) / 1000000)
            $gaps += $d; if ($d -gt $worst) { $worst = $d }
        }
        $prev = $pts
    }
    $r.Dispose(); $s.Dispose()
    $sorted = $gaps | Sort-Object
    Say ("  {0}: chunks={1} keyframes={2} worst delta={3} ms" -f $dump.Name, $n, $kf, $worst)
    if ($sorted) { Say ("  delta median={0} ms  p99={1} ms  over-100ms={2}" -f $sorted[[int]($sorted.Count / 2)], $sorted[[int]($sorted.Count * 0.99)], ($gaps | Where-Object { $_ -gt 100 }).Count) }
}

# --- 9. what the client saw ---------------------------------------------------------------------
Say '--- client gap ---'
$fps = Get-Content $clientLog -ErrorAction SilentlyContinue |
    Where-Object { $_ -match 'stats:' } |
    ForEach-Object { if ($_ -match '(\d+)\s*fps') { [int]$Matches[1] } }
if (-not $fps) { Say '  no stats lines (client never presented)' }
else {
    $run = 0; $worstRun = 0
    foreach ($v in $fps) { if ($v -eq 0) { $run++; if ($run -gt $worstRun) { $worstRun = $run } } else { $run = 0 } }
    Say ("  {0} one-second windows, {1} at 0 fps, longest run {2} s" -f $fps.Count, ($fps | Where-Object { $_ -eq 0 }).Count, $worstRun)
}

Set-ItemProperty -Path $svcEnv -Name 'Environment' -Type MultiString -Value @()
Say "done - $runLog / $pollLog / $clientLog / $dumpDir"
