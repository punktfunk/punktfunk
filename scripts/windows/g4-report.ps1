<#
.SYNOPSIS
  Cut a g4-soak.ps1 run into the six section 5 pass criteria of windows-video-plane-overhaul.md.

.DESCRIPTION
  Reads live-g4-<Tag>.log (leg markers), host.log over the run window, g4-<Tag>-status.jsonl
  (timed control-plane samples) and the pre/post snapshots, and prints one section per criterion
  with the numbers behind it. Criterion 1 attributes every gap of -GapBar or more: it is outside
  our path only when the co-timed stall line says compose-silence with zero DXGI presents AND the
  DxgKrnl ETW bracket names a display DDI. Everything else counts against us, and a gap with no
  ETW evidence at all is reported as UNATTRIBUTED, never as excluded.

  Run after the soak:  pwsh -NoProfile -File C:\Users\Public\g4-report.ps1 -Tag g4n
#>
param(
  [string]$Tag = 'g4n',
  [string]$HostLog = 'C:\ProgramData\punktfunk\logs\host.log',
  # Where the soak's logs live. Point it at a copied-off directory to cut a run elsewhere.
  [string]$Root = 'C:\Users\Public',
  # Gaps this long or longer are criterion 1's subject (ms).
  [int]$GapBar = 2000,
  # Cut a synthetic run with known answers and assert them. Writes nothing outside the temp dir.
  [switch]$SelfCheck
)

if ($SelfCheck) {
  $d = Join-Path ([IO.Path]::GetTempPath()) ("g4sc-" + [Guid]::NewGuid().ToString('N').Substring(0, 8))
  New-Item -ItemType Directory -Path $d -Force | Out-Null
  @(
    '2026-09-03T10:00:00Z EVENT kind=cycle-start n=1 profile=pf-g4-a'
    '2026-09-03T10:00:05Z EVENT kind=cycle-streaming n=1 after_ms=5000'
    '2026-09-03T10:01:00Z EVENT kind=resize-end n=1 i=1 live=2560x1440 last_resize_ms=540'
    '2026-09-03T10:02:00Z EVENT kind=lock-end n=1 ok=True unlock_ms=1200 recover_ms=800 class=healthy'
    '2026-09-03T10:03:00Z EVENT kind=sleep-skip reason=no-armed-wake-timer'
    '2026-09-03T10:04:00Z EVENT kind=run-end cycles=1 resizes=1 locks=1 sleep=False'
  ) | Set-Content (Join-Path $d "live-g4-sc.log")
  @(
    '2026-09-03T10:00:01.0Z  INFO pf_capture::windows::idd_push::open: IDD push: PUNKTFUNK_IDD_DIAG is ON au_dump_dir=C:\Windows\Temp'
    # 790 ms hole right after the client came up: the startup artefact, must not be counted.
    '2026-09-03T10:00:06.0Z DEBUG pf_capture::windows::idd_push::stall: IDD-push capture stall - x gap_ms=790 verdict=compose-silence (dwm) etw=none etw_presents=0 etw_queue_adds=0 cursor_moved_px_during_gap=0 flow_dwm_only=true max_heartbeat_age_ms=4'
    # Ours: the drain heartbeat went stale for most of a 4 s hole.
    '2026-09-03T10:01:30.0Z DEBUG pf_capture::windows::idd_push::stall: IDD-push capture stall - x gap_ms=4000 verdict=driver-worker-stalled (heartbeat silent) etw=none etw_presents=12 etw_queue_adds=0 cursor_moved_px_during_gap=90 flow_dwm_only=false max_heartbeat_age_ms=3900'
    # Outside: DWM presented nothing and DxgKrnl brackets a display DDI.
    '2026-09-03T10:02:30.0Z DEBUG pf_capture::windows::idd_push::stall: IDD-push capture stall - x gap_ms=3000 verdict=compose-silence (the pool took no frame) etw=QueryChildStatus*2(max 900ms) etw_presents=0 etw_queue_adds=0 cursor_moved_px_during_gap=70 flow_dwm_only=false max_heartbeat_age_ms=6'
    '2026-09-03T10:01:31.0Z  INFO punktfunk_host::native::control: pipeline rebuilt in place - telling the client the stream had a gap gap_ms=120'
    '2026-09-03T10:01:32.0Z  WARN pf_capture::windows::idd_push::health: IDD push: recovery stage stage=PresentationReset outcome=Applied'
    '2026-09-03T10:01:33.0Z  INFO pf_capture::windows::idd_push::health: IDD push: recovery episode closed outage_ms=900'
    '2026-09-03T10:01:34.0Z  INFO punktfunk_host::bringup: session-transition trace kind=reassert-recover total_ms=556 stages=display_resized+423'
  ) | Set-Content (Join-Path $d "hostsc.log")
  @(
    '{"t":"2026-09-03T10:00:02","ms":31,"http":200,"body":{"display":{"topology_generation":3,"pnp_leases":0}}}'
    '{"t":"2026-09-03T10:01:32","ms":1400,"http":200,"body":{"capture":{"last_episode":{"stages":[{"stage":"presentation_reset","outcome":"applied","took_ms":81}]}}}}'
    '{"t":"2026-09-03T10:01:40","ms":22,"http":0,"body":null}'
  ) | Set-Content (Join-Path $d "g4-sc-status.jsonl")
  $r = & $PSCommandPath -Tag sc -Root $d -HostLog (Join-Path $d "hostsc.log")
  $text = $r -join "`n"
  $fail = 0
  foreach ($want in @(
      'client-startup artefacts excluded: 1',
      'over-bar gaps: ours=1 outside=1 idle=0 unattributed=0',
      'presentation_reset firings at or over 1 s: 0',
      'holes with a stale drain heartbeat (driver-side): 1',
      'over-1s-or-failed=2')) {
    if ($text -notmatch [regex]::Escape($want)) { Write-Output "SELFCHECK FAIL: missing '$want'"; $fail++ }
  }
  $stageCost = ($text -match 'stage cost: presentation_reset\s+applied\s+81 ms')
  if (-not $stageCost) { Write-Output 'SELFCHECK FAIL: rung cost not read from the status samples'; $fail++ }
  Remove-Item $d -Recurse -Force
  if ($fail) { Write-Output "SELFCHECK: $fail assertion(s) failed"; exit 1 }
  Write-Output 'SELFCHECK OK'
  exit 0
}

$root = $Root
$runLog = "$root\live-g4-$Tag.log"
$statusPath = "$root\g4-$Tag-status.jsonl"
$out = "$root\g4-report-$Tag.txt"
if (-not (Test-Path $runLog)) { Write-Output "no run log $runLog"; exit 1 }

function Emit([string]$s) { $s | Add-Content $out }
function Field([string]$line, [string]$name) {
  if ($line -match ($name + '="([^"]*)"')) { return $Matches[1] }
  if ($line -match ($name + '=(\S+)')) { return $Matches[1] }
  return ''
}

$events = Get-Content $runLog | Where-Object { $_ -match 'EVENT kind=' } | ForEach-Object {
  $t = ($_ -split ' ')[0].TrimEnd('Z')
  [pscustomobject]@{ t = $t; kind = (Field $_ 'kind'); line = $_ }
}
if (-not $events) { Write-Output 'no EVENT markers - the run never started'; exit 1 }
$from = ($events | Select-Object -First 1).t
$to = ($events | Select-Object -Last 1).t
$streamMarks = @($events | Where-Object { $_.kind -eq 'cycle-streaming' } | ForEach-Object { $_.t })

"== G4 report $Tag  window ${from}Z .. ${to}Z" | Set-Content $out

$hl = Get-Content $HostLog | ForEach-Object { $_ -replace "\x1b\[[0-9;]*m", "" } |
  Where-Object { $_ -match '^\d{4}-' -and $_ -ge $from -and $_ -le ($to + 'Z') }
Emit "host.log lines in window: $(($hl | Measure-Object).Count)"

# The ETW/probe leg is criterion 1's whole evidence base; a run without it cannot answer it.
$diagOn = ($hl | Where-Object { $_ -match 'PUNKTFUNK_IDD_DIAG is ON' } | Measure-Object).Count
$etwOff = ($hl | Where-Object { $_ -match 'DxgKrnl ETW session unavailable' } | Measure-Object).Count
Emit "diagnostics: IDD_DIAG sessions=$diagOn etw_unavailable_warnings=$etwOff"
if ($diagOn -eq 0) { Emit 'WARNING: no session ran with PUNKTFUNK_IDD_DIAG - criterion 1 cannot be attributed' }

$stalls = $hl | Where-Object { $_ -match 'IDD-push capture stall' } | ForEach-Object {
  $etw = if ($_ -match 'etw=(.*?)\s+etw_presents=') { $Matches[1] } else { Field $_ 'etw' }
  [pscustomobject]@{
    t          = ($_ -split ' ')[0]
    gap_ms     = [int](Field $_ 'gap_ms')
    verdict    = (Field $_ 'verdict')
    etw        = $etw
    presents   = (Field $_ 'etw_presents')
    queue_adds = (Field $_ 'etw_queue_adds')
    dwm_only   = (Field $_ 'flow_dwm_only')
    hb_ms      = (Field $_ 'max_heartbeat_age_ms')
    cursor_px  = (Field $_ 'cursor_moved_px_during_gap')
  }
}

$rebuilt = $hl | Where-Object { $_ -match 'pipeline rebuilt in place' } | ForEach-Object {
  [pscustomobject]@{ t = ($_ -split ' ')[0]; gap_ms = [int](Field $_ 'gap_ms') }
}

# The loopback client's own startup probe leaves a ~790 ms compose-silence hole per session.
# It is a test artefact of this client, so it is counted apart, never against the driver.
function Is-StartupArtefact($ts, $ms) {
  if ($ms -lt 550 -or $ms -gt 1100) { return $false }
  $t = [datetime]::Parse($ts.TrimEnd('Z'))
  foreach ($m in $streamMarks) {
    $d = ($t - [datetime]::Parse($m)).TotalSeconds
    if ($d -ge -2 -and $d -le 8) { return $true }
  }
  return $false
}

$all = @()
foreach ($r in $rebuilt) { $all += [pscustomobject]@{ t = $r.t; ms = $r.gap_ms; src = 'client-visible' } }
foreach ($s in $stalls) { $all += [pscustomobject]@{ t = $s.t; ms = $s.gap_ms; src = 'capture-hole' } }
$artefacts = @($all | Where-Object { Is-StartupArtefact $_.t $_.ms })
$real = @($all | Where-Object { -not (Is-StartupArtefact $_.t $_.ms) })

Emit ''
Emit "-- 1. no client-visible gap over $GapBar ms inside our path"
Emit "gaps seen: $($all.Count) (client-visible $($rebuilt.Count), capture holes $($stalls.Count)); client-startup artefacts excluded: $($artefacts.Count)"
$big = @($real | Where-Object { $_.ms -ge $GapBar } | Sort-Object ms -Descending)
$ours = 0; $outside = 0; $unattributed = 0; $idle = 0
foreach ($g in $big) {
  $gt = [datetime]::Parse($g.t.TrimEnd('Z'))
  $near = $stalls | Where-Object { [Math]::Abs(([datetime]::Parse($_.t.TrimEnd('Z')) - $gt).TotalSeconds) -le 3 } | Select-Object -First 1
  $verdict = 'UNATTRIBUTED'; $why = 'no co-timed stall line'
  if ($near) {
    # The DxgKrnl bracket string is empty or the word `unavailable`/`none` when no display DDI
    # was servicing; anything else names one, which is the adapter pause section 5 asks for.
    $adapterPause = ($near.etw -and $near.etw -notmatch '^(unavailable|none|)$')
    if ($near.verdict -like 'driver-worker-stalled*') { $verdict = 'OURS' }
    elseif ($near.verdict -like 'damage-idle*') { $verdict = 'IDLE' }
    elseif ($near.presents -eq '') { $verdict = 'UNATTRIBUTED' }
    elseif ($near.presents -eq '0' -and $adapterPause) { $verdict = 'OUTSIDE' }
    else { $verdict = 'OURS' }
    $why = "stall_verdict=$($near.verdict) presents=$($near.presents) queue_adds=$($near.queue_adds) hb_ms=$($near.hb_ms) etw=$($near.etw)"
  }
  switch ($verdict) {
    'OURS' { $ours++ }
    'OUTSIDE' { $outside++ }
    'IDLE' { $idle++ }
    default { $unattributed++ }
  }
  Emit ("  {0} {1,7} ms {2,-13} {3} {4}" -f $g.t, $g.ms, $g.src, $verdict, $why)
}
Emit "over-bar gaps: ours=$ours outside=$outside idle=$idle unattributed=$unattributed"
Emit "criterion 1 PASSES only when ours=0 and every unattributed one is read by a human"

Emit ''
Emit '-- 2. p99 of the remaining gaps under 500 ms, median under 100 ms'
$rest = @($real | Where-Object { $_.ms -lt $GapBar } | ForEach-Object { $_.ms } | Sort-Object)
if ($rest.Count -eq 0) { Emit 'n=0 (no sub-bar gaps)' }
else {
  $p = { param($q) $rest[[Math]::Min($rest.Count - 1, [int][Math]::Ceiling($q * $rest.Count) - 1)] }
  Emit "n=$($rest.Count) median=$(& $p 0.5) ms p95=$(& $p 0.95) ms p99=$(& $p 0.99) ms max=$($rest[-1]) ms"
}

Emit ''
Emit '-- 3. every rung cost less than the gap it closed; PresentationReset under 1 s'
$stages = $hl | Where-Object { $_ -match 'IDD push: recovery stage' } | ForEach-Object {
  "  $(($_ -split ' ')[0]) stage=$(Field $_ 'stage') outcome=$(Field $_ 'outcome')"
}
$closed = $hl | Where-Object { $_ -match 'recovery episode closed' } | ForEach-Object {
  "  $(($_ -split ' ')[0]) outage_ms=$(Field $_ 'outage_ms')"
}
$exhausted = @($hl | Where-Object { $_ -match 'recovery ladder exhausted' })
Emit "rungs fired: $(($stages | Measure-Object).Count)  episodes closed: $(($closed | Measure-Object).Count)  ladders exhausted: $($exhausted.Count)"
$stages | ForEach-Object { Emit $_ }
$closed | ForEach-Object { Emit $_ }
# The per-rung cost only exists as a number in the status surface's last_episode.
$episodes = @()
if (Test-Path $statusPath) {
  Get-Content $statusPath | ForEach-Object {
    if ($_ -match '"last_episode"') {
      foreach ($m in [regex]::Matches($_, '"stage"\s*:\s*"(\w+)"\s*,\s*"outcome"\s*:\s*"(\w+)"\s*,\s*"took_ms"\s*:\s*(\d+)')) {
        $episodes += [pscustomobject]@{ stage = $m.Groups[1].Value; outcome = $m.Groups[2].Value; took = [int]$m.Groups[3].Value }
      }
    }
  }
}
$episodes = $episodes | Sort-Object stage, outcome, took -Unique
foreach ($e in $episodes) { Emit ("  stage cost: {0,-20} {1,-12} {2} ms" -f $e.stage, $e.outcome, $e.took) }
$pr = @($episodes | Where-Object { $_.stage -eq 'presentation_reset' -and $_.took -ge 1000 })
Emit "presentation_reset firings at or over 1 s: $($pr.Count)"
$transitions = $hl | Where-Object { $_ -match 'session-transition trace' } | ForEach-Object {
  "  $(($_ -split ' ')[0]) kind=$(Field $_ 'kind') total_ms=$(Field $_ 'total_ms')"
}
$transitions | ForEach-Object { Emit $_ }

Emit ''
Emit '-- 4. no compose hitch attributable to the driver'
$worker = @($stalls | Where-Object { $_.verdict -like 'driver-worker-stalled*' })
$degraded = $hl | Where-Object { $_ -match 'recovered from a degraded stretch' } | ForEach-Object {
  "  $(($_ -split ' ')[0]) degraded_ms=$(Field $_ 'degraded_ms') holes=$(Field $_ 'holes') worst_hole_ms=$(Field $_ 'worst_hole_ms') present_to_arrival_ms=$(Field $_ 'present_to_arrival_ms')"
}
Emit "holes with a stale drain heartbeat (driver-side): $($worker.Count)"
$worker | Select-Object -First 12 | ForEach-Object { Emit ("  {0} {1} ms hb={2} ms etw={3}" -f $_.t, $_.gap_ms, $_.hb_ms, $_.etw) }
Emit "degraded stretches: $(($degraded | Measure-Object).Count)"
$degraded | ForEach-Object { Emit $_ }
Emit 'LIMIT: the stall watch floors at 150 ms, so a hitch of 2-9 frame periods is invisible here.'
Emit 'A true per-frame qpc_pts cadence check needs a host-side instrument that does not exist yet.'

Emit ''
Emit '-- 5. control plane answers within 1 s throughout'
if (-not (Test-Path $statusPath)) { Emit 'no status samples - criterion 5 unevidenced' }
else {
  $ms = @(); $bad = 0; $n = 0
  Get-Content $statusPath | ForEach-Object {
    $n++
    if ($_ -match '"ms":(\d+),"http":(\d+)') {
      $ms += [int]$Matches[1]
      if ([int]$Matches[2] -ne 200 -or [int]$Matches[1] -gt 1000) { $bad++ }
    }
  }
  $sorted = $ms | Sort-Object
  $q = { param($x) $sorted[[Math]::Min($sorted.Count - 1, [int][Math]::Ceiling($x * $sorted.Count) - 1)] }
  Emit "samples=$n median=$(& $q 0.5) ms p99=$(& $q 0.99) ms max=$($sorted[-1]) ms over-1s-or-failed=$bad"
  Get-Content $statusPath | Where-Object { $_ -match '"ms":(\d+)' -and [int]$Matches[1] -gt 1000 } |
    Select-Object -First 10 | ForEach-Object { Emit ("  slow: " + $_.Substring(0, [Math]::Min(120, $_.Length))) }
  $cycles = @($hl | Where-Object { $_ -match 'DriverCycle|driver_cycle' })
  Emit "driver cycles in window: $($cycles.Count) (each one must sit inside the samples above)"
}

Emit ''
Emit '-- 6. state restored after every recovery episode'
foreach ($s in 'pre', 'post') {
  $f = "$root\g4-$Tag-snap-$s.json"
  if (Test-Path $f) {
    $j = Get-Content $f -Raw | ConvertFrom-Json
    $modes = ([regex]::Matches($j.displays, '"mode"\s*:\s*"([^"]+)"') | ForEach-Object { $_.Groups[1].Value }) -join ' '
    $topo = ([regex]::Matches($j.displays, '"topology"\s*:\s*"([^"]+)"') | ForEach-Object { $_.Groups[1].Value }) -join ' '
    $gen = if ($j.status -match '"topology_generation"\s*:\s*(\d+)') { $Matches[1] } else { '?' }
    $leases = if ($j.status -match '"pnp_leases"\s*:\s*(\d+)') { $Matches[1] } else { '?' }
    $screens = ($j.screens | ForEach-Object { "$($_.w)x$($_.h)@$($_.x),$($_.y)$(if ($_.primary) { '*' })" }) -join ' '
    Emit "  $s : displays=[$modes] topology=[$topo] topology_generation=$gen pnp_leases=$leases screens=[$screens]"
  } else { Emit "  $s : snapshot missing" }
}
$hdr = $hl | Where-Object { $_ -match 'session_hdr=|advanced color' } | Select-Object -Last 6 |
  ForEach-Object { "  hdr: " + $_.Substring(0, [Math]::Min(200, $_.Length)) }
$hdr | ForEach-Object { Emit $_ }
Emit 'Compare the pre and post lines by eye: the modes, the topology word and pnp_leases must match.'

Emit ''
Emit '-- leg tally'
foreach ($k in 'cycle-start', 'resize-end', 'lock-end', 'sleep-begin', 'sleep-end', 'sleep-skip', 'load-start') {
  Emit ("  {0,-14} {1}" -f $k, (($events | Where-Object { $_.kind -eq $k } | Measure-Object).Count))
}
$events | Where-Object { $_.kind -in @('lock-end', 'sleep-end', 'sleep-skip', 'resize-end') } |
  ForEach-Object { Emit ("  " + $_.line) }
$errs = @($hl | Where-Object { $_ -match ' ERROR ' })
Emit "host ERROR lines: $($errs.Count)"
$errs | Select-Object -First 10 | ForEach-Object { Emit ("  " + $_.Substring(0, [Math]::Min(240, $_.Length))) }

Get-Content $out
