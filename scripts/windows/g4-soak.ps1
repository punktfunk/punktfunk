<#
.SYNOPSIS
  Gate G4 stall-immunity soak (windows-video-plane-overhaul.md section 5) for one vendor.

.DESCRIPTION
  Runs the section 5 matrix unattended on a box with the driver-encode host installed: a real client
  under a GPU load generator at the session resolution, N reconnects, in-place resizes,
  lock/unlock cycles on the secure desktop, and one sleep/resume. Every leg writes an EVENT
  marker into live-g4-<Tag>.log; a status poller writes one timed /api/v1/status sample every
  two seconds into g4-<Tag>-status.jsonl. g4-report.ps1 cuts both into the six section 5 criteria.

  Detached, so it survives the ssh drop and the sleep leg (kick-task.ps1 is not in this repo):
    kick-task.ps1 -Name pf-g4 -Script C:\Users\Public\g4-soak.ps1 -ScriptArgs "-Tag g4n -Minutes 240"
  -ScriptArgs carries no commas and no colons, so list arguments use '/' as the separator.

  Sets PUNKTFUNK_IDD_DIAG in host.env for the run: without it the host starts no DxgKrnl ETW
  session and criterion 1 has nothing to attribute a gap with. host.env is restored at the end.

.EXAMPLE
  Full NVIDIA leg, four hours, every leg:
    kick-task.ps1 -Name pf-g4n -Script C:\Users\Public\g4-soak.ps1 -ScriptArgs "-Tag g4n -Minutes 240"

.EXAMPLE
  AMD leg (AMF on the Radeon iGPU, same box):
    kick-task.ps1 -Name pf-g4a -Script C:\Users\Public\g4-soak.ps1 -ScriptArgs "-Tag g4a -Minutes 240 -Vendor amd"

.EXAMPLE
  One leg only, no GPU load, short:
    kick-task.ps1 -Name pf-g4L -Script C:\Users\Public\g4-soak.ps1 -ScriptArgs "-Tag lock -Minutes 20 -Legs lock -Reconnects 10 -Locks 10"
    kick-task.ps1 -Name pf-g4S -Script C:\Users\Public\g4-soak.ps1 -ScriptArgs "-Tag slp -Minutes 10 -Legs sleep -Reconnects 3"

.NOTES
  The sleep leg SKIPS itself unless Windows lists this run's wake task under `powercfg
  /waketimers`, so a box that cannot wake itself is never put to sleep. Verify once by hand
  before the real run: arm any -WakeToRun task and check that it appears there.
#>
param(
  # Log/state suffix. One tag per vendor leg (g4n / g4a).
  [string]$Tag = 'g4n',
  [int]$Minutes = 240,
  # nvidia pins NVENC on the discrete card; amd pins AMF on the Radeon iGPU (same box).
  [string]$Vendor = 'nvidia',
  # 'all', or a '/'-separated subset of load/reconnect/resize/lock/sleep.
  [string]$Legs = 'all',
  [int]$Reconnects = 100,
  [int]$Resizes = 20,
  [int]$Locks = 10,
  [int]$LockHoldSecs = 20,
  [int]$SleepHoldSecs = 90,
  # furmark | none.
  [string]$Load = 'furmark',
  # FurMark --gpu-index; -1 leaves the choice to FurMark.
  [int]$LoadGpuIndex = -1,
  # Continue an interrupted run from its state file instead of starting over.
  [switch]$Resume
)

$ErrorActionPreference = 'Continue'
$root = 'C:\Users\Public'
$log = "$root\live-g4-$Tag.log"
$statusPath = "$root\g4-$Tag-status.jsonl"
$statePath = "$root\g4-$Tag.state"
$hostLog = 'C:\ProgramData\punktfunk\logs\host.log'
$envPath = 'C:\ProgramData\punktfunk\host.env'
$envBak = "$root\g4-$Tag-host.env.bak"
$client = "$root\punktfunk-native\target\release\punktfunk-session.exe"
$furmark = 'C:\Program Files\Geeks3D\FurMark2_x64\furmark.exe'
$profiles = Join-Path $env:APPDATA 'punktfunk\client-profiles.json'
$profilesBak = "$root\g4-$Tag-client-profiles.bak"
$unlockTask = "pf-g4-unlock-$Tag"
$wakeTask = "pf-g4-wake-$Tag"

# Two sizes the resize leg alternates between; both must be modes the driver offers.
$modeA = @{ w = 1920; h = 1080; hz = 60; id = "pf-g4-a" }
$modeB = @{ w = 2560; h = 1440; hz = 60; id = "pf-g4-b" }

Add-Type -AssemblyName System.Windows.Forms -ErrorAction SilentlyContinue
Add-Type -Name W -Namespace PfG4 -MemberDefinition @'
[DllImport("user32.dll", SetLastError=true)] public static extern IntPtr OpenInputDesktop(uint flags, bool inherit, uint access);
[DllImport("user32.dll", SetLastError=true)] public static extern bool CloseDesktop(IntPtr h);
[DllImport("user32.dll", SetLastError=true, CharSet=CharSet.Unicode)] public static extern bool GetUserObjectInformationW(IntPtr h, int index, System.Text.StringBuilder info, int len, out int needed);
[DllImport("user32.dll")] public static extern bool LockWorkStation();
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
[DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
[DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
[DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
public delegate bool EnumProc(IntPtr h, IntPtr p);
public struct RECT { public int left, top, right, bottom; }
'@ -ErrorAction SilentlyContinue

function Say([string]$text) {
  "$((Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ss'))Z $text" | Add-Content $log
}

# One machine-readable leg boundary. The report cutter attributes every gap to the leg it fell in.
function Mark([string]$kind, [string]$fields = '') {
  Say "EVENT kind=$kind $fields"
}

# The input desktop is Winlogon's (UAC / lock screen): OpenInputDesktop is refused there.
function Test-SecureDesktop {
  $h = [PfG4.W]::OpenInputDesktop(0, $false, 0x0100)
  if ($h -eq [IntPtr]::Zero) { return $true }
  [PfG4.W]::CloseDesktop($h) | Out-Null
  return $false
}

function Get-MgmtToken {
  (Get-Content -Raw 'C:\ProgramData\punktfunk\mgmt-token').Trim() -replace '^[A-Z_]+=', ''
}

# GET one management endpoint, timed. `ms` is the whole round trip - criterion 5's number.
function Get-Mgmt([string]$path, [string]$token) {
  $sw = [Diagnostics.Stopwatch]::StartNew()
  try {
    $r = Invoke-WebRequest -Uri "https://127.0.0.1:47990$path" -Headers @{ Authorization = "Bearer $token" } -SkipCertificateCheck -TimeoutSec 5
    $sw.Stop()
    return @{ ms = [int]$sw.Elapsed.TotalMilliseconds; http = [int]$r.StatusCode; body = $r.Content }
  } catch {
    $sw.Stop()
    return @{ ms = [int]$sw.Elapsed.TotalMilliseconds; http = 0; body = "" }
  }
}

function Save-Snapshot([string]$name, [string]$token) {
  $screens = [System.Windows.Forms.Screen]::AllScreens | ForEach-Object {
    @{ device = $_.DeviceName; primary = $_.Primary; x = $_.Bounds.X; y = $_.Bounds.Y; w = $_.Bounds.Width; h = $_.Bounds.Height }
  }
  $snap = @{
    at       = (Get-Date).ToUniversalTime().ToString('o')
    status   = (Get-Mgmt '/api/v1/status' $token).body
    displays = (Get-Mgmt '/api/v1/display/state' $token).body
    monitors = (Get-Mgmt '/api/v1/display/monitors' $token).body
    screens  = $screens
  }
  $snap | ConvertTo-Json -Depth 8 -Compress | Set-Content "$root\g4-$Tag-snap-$name.json" -Encoding utf8
  Mark 'snapshot' "name=$name"
}

# The streamed display: the non-primary head, preferring one whose size is a session mode.
function Get-VirtualRect {
  $all = [System.Windows.Forms.Screen]::AllScreens
  $cand = $all | Where-Object { -not $_.Primary }
  if (-not $cand) { $cand = $all }
  $sized = $cand | Where-Object {
    ($_.Bounds.Width -eq $modeA.w -and $_.Bounds.Height -eq $modeA.h) -or
    ($_.Bounds.Width -eq $modeB.w -and $_.Bounds.Height -eq $modeB.h)
  }
  $pick = if ($sized) { $sized | Select-Object -First 1 } else { $cand | Select-Object -First 1 }
  if (-not $pick) { $pick = [System.Windows.Forms.Screen]::PrimaryScreen }
  return $pick.Bounds
}

function Get-MainWindow([int]$procId) {
  $script:found = [IntPtr]::Zero
  $cb = [PfG4.W+EnumProc] {
    param($h, $p)
    $owner = 0
    [PfG4.W]::GetWindowThreadProcessId($h, [ref]$owner) | Out-Null
    if ($owner -eq $procId -and [PfG4.W]::IsWindowVisible($h)) {
      $r = New-Object PfG4.W+RECT
      [PfG4.W]::GetWindowRect($h, [ref]$r) | Out-Null
      if (($r.right - $r.left) -gt 200 -and ($r.bottom - $r.top) -gt 200) { $script:found = $h; return $false }
    }
    return $true
  }
  [PfG4.W]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
  return $script:found
}

# SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW
$SWP = 0x0004 -bor 0x0010 -bor 0x0040

function Move-Furmark {
  $p = Get-Process furmark -ErrorAction SilentlyContinue | Select-Object -First 1
  if (-not $p) { return }
  $h = Get-MainWindow $p.Id
  if ($h -eq [IntPtr]::Zero) { return }
  $b = Get-VirtualRect
  # Inset so the title bar stays on the virtual display after a mode change shrinks it.
  [PfG4.W]::SetWindowPos($h, [IntPtr]::Zero, $b.X + 8, $b.Y + 8, [Math]::Min(1280, $b.Width - 16), [Math]::Min(720, $b.Height - 16), $SWP) | Out-Null
}

function Start-Load([int]$seconds) {
  if ($Load -ne 'furmark') { return }
  if (Get-Process furmark -ErrorAction SilentlyContinue) { return }
  if (-not (Test-Path $furmark)) { Say "LOAD-MISSING $furmark"; return }
  $b = Get-VirtualRect
  $a = @('--demo', 'furmark-vk', '--width', [Math]::Min(1280, $b.Width), '--height', [Math]::Min(720, $b.Height),
    '--max-time', $seconds, '--no-score-box', '--disable-demo-options', '--disable-traces')
  if ($LoadGpuIndex -ge 0) { $a += @('--gpu-index', $LoadGpuIndex) }
  Start-Process -FilePath $furmark -ArgumentList $a -WorkingDirectory (Split-Path $furmark) | Out-Null
  Start-Sleep 6
  Move-Furmark
  Mark 'load-start' "demo=furmark-vk gpu_index=$LoadGpuIndex"
}

# host.env wins over the machine environment (the service set_var's it), so the run's knobs
# go in the file. Every key we touch is restored from $envBak in Restore-Box.
function Set-HostEnv([hashtable]$kv) {
  if (-not (Test-Path $envBak)) { Copy-Item $envPath $envBak -Force }
  $keep = Get-Content $envPath | Where-Object {
    $k = ($_ -split '=', 2)[0].Trim()
    -not ($kv.Keys -contains $k)
  }
  $out = @($keep) + ($kv.Keys | ForEach-Object { "$_=$($kv[$_])" })
  $out | Set-Content $envPath -Encoding ascii
  Restart-Service PunktfunkHost -Force -ErrorAction SilentlyContinue
  Start-Sleep 15
  Say "host.env applied: $(($kv.Keys | ForEach-Object { "$_=$($kv[$_])" }) -join ' ') service=$((Get-Service PunktfunkHost).Status)"
}

function Restore-Box {
  if (Test-Path $envBak) { Copy-Item $envBak $envPath -Force; Restart-Service PunktfunkHost -Force -ErrorAction SilentlyContinue }
  if (Test-Path $profilesBak) { Copy-Item $profilesBak $profiles -Force } else { Remove-Item $profiles -ErrorAction SilentlyContinue }
  Get-Process furmark -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
  Get-Process punktfunk-session -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
  foreach ($t in $unlockTask, $wakeTask) { Unregister-ScheduledTask $t -Confirm:$false -ErrorAction SilentlyContinue }
  Say "restored host.env + client profiles"
}

# --- lock/unlock -------------------------------------------------------------------------
# LockWorkStation puts the console on the Winlogon secure desktop; only SYSTEM can bring it
# back without a password, via `tscon <session> /dest:console`. That is a second task.
function Invoke-LockCycle([int]$n, [string]$token) {
  $sid = (Get-Process -Id $PID).SessionId
  Mark 'lock-begin' "n=$n session=$sid"
  [PfG4.W]::LockWorkStation() | Out-Null
  $sw = [Diagnostics.Stopwatch]::StartNew()
  while ($sw.Elapsed.TotalSeconds -lt 20 -and -not (Test-SecureDesktop)) { Start-Sleep -Milliseconds 500 }
  $locked = Test-SecureDesktop
  Mark 'lock-secure' "n=$n locked=$locked after_ms=$([int]$sw.Elapsed.TotalMilliseconds) class=$(Get-CaptureClass $token)"
  if (-not $locked) { Mark 'lock-end' "n=$n ok=False reason=never-locked"; return $true }
  Start-Sleep $LockHoldSecs
  $sw = [Diagnostics.Stopwatch]::StartNew()
  Start-ScheduledTask $unlockTask -ErrorAction SilentlyContinue
  while ($sw.Elapsed.TotalSeconds -lt 40 -and (Test-SecureDesktop)) { Start-Sleep -Milliseconds 500 }
  if (Test-SecureDesktop) {
    Start-ScheduledTask $unlockTask -ErrorAction SilentlyContinue
    while ($sw.Elapsed.TotalSeconds -lt 90 -and (Test-SecureDesktop)) { Start-Sleep -Milliseconds 500 }
  }
  $ok = -not (Test-SecureDesktop)
  $unlockMs = [int]$sw.Elapsed.TotalMilliseconds
  # Recovery proof: the capture health class has to leave secure_desktop on its own.
  $sw2 = [Diagnostics.Stopwatch]::StartNew()
  while ($ok -and $sw2.Elapsed.TotalSeconds -lt 30 -and (Get-CaptureClass $token) -eq 'secure_desktop') { Start-Sleep -Milliseconds 500 }
  Mark 'lock-end' "n=$n ok=$ok unlock_ms=$unlockMs recover_ms=$([int]$sw2.Elapsed.TotalMilliseconds) class=$(Get-CaptureClass $token)"
  return $ok
}

function Get-CaptureClass([string]$token) {
  $r = Get-Mgmt '/api/v1/status' $token
  if ($r.http -ne 200) { return 'unreachable' }
  if ($r.body -match '"class"\s*:\s*"([a-z_]+)"') { return $Matches[1] }
  return 'none'
}

# --- sleep/resume ------------------------------------------------------------------------
# A wake-to-run task is the only lever that brings this box back on its own. If Windows does
# not list it under `powercfg /waketimers` the leg is SKIPPED rather than risking a dark box.
function Invoke-SleepCycle([string]$token) {
  $states = (& powercfg /a) -join ' '
  if ($states -notmatch 'S3') { Mark 'sleep-skip' 'reason=no-s3'; return }
  $ac = (& powercfg /query SCHEME_CURRENT SUB_SLEEP RTCWAKE) |
    Where-Object { $_ -match 'Wechselstrom|AC Power Setting' } | Select-Object -First 1
  if ($ac -match '0x00000000') { Mark 'sleep-skip' 'reason=wake-timers-disabled'; return }
  $at = (Get-Date).AddSeconds($SleepHoldSecs)
  $act = New-ScheduledTaskAction -Execute (Get-Command pwsh).Source -Argument "-NoProfile -File `"$root\g4-mark.ps1`" -Tag $Tag -Kind wake"
  $trg = New-ScheduledTaskTrigger -Once -At $at
  $set = New-ScheduledTaskSettingsSet -WakeToRun -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable
  $prn = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
  Unregister-ScheduledTask $wakeTask -Confirm:$false -ErrorAction SilentlyContinue
  Register-ScheduledTask -TaskName $wakeTask -Action $act -Trigger $trg -Settings $set -Principal $prn -Force | Out-Null
  Start-Sleep 3
  $timers = (& powercfg /waketimers) -join ' '
  if ($timers -notmatch [regex]::Escape($wakeTask)) {
    Mark 'sleep-skip' "reason=no-armed-wake-timer timers=$($timers.Substring(0, [Math]::Min(160, $timers.Length)))"
    Unregister-ScheduledTask $wakeTask -Confirm:$false -ErrorAction SilentlyContinue
    return
  }
  Mark 'sleep-begin' "wake_at=$($at.ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ss'))Z hold_s=$SleepHoldSecs"
  $before = Get-Date
  [System.Windows.Forms.Application]::SetSuspendState([System.Windows.Forms.PowerState]::Suspend, $false, $false) | Out-Null
  # Execution stops here until the box resumes; the wall clock is the only witness of the gap.
  while ((Get-Date) -lt $at.AddSeconds(20)) { Start-Sleep 5 }
  $lastwake = (& powercfg /lastwake) -join ' '
  Mark 'sleep-end' "asleep_s=$([int]((Get-Date) - $before).TotalSeconds) lastwake=$($lastwake -replace '\s+', '+')"
  Unregister-ScheduledTask $wakeTask -Confirm:$false -ErrorAction SilentlyContinue
  # The service may have lost the adapter across S3; give it a settling window before the next cycle.
  Start-Sleep 30
  Mark 'sleep-recovered' "service=$((Get-Service PunktfunkHost).Status) class=$(Get-CaptureClass $token)"
}

# --- preflight ---------------------------------------------------------------------------
$legList = if ($Legs -eq 'all') { @('load', 'reconnect', 'resize', 'lock', 'sleep') } else { $Legs.Split('/') }
$doResize = $legList -contains 'resize'
$doLock = $legList -contains 'lock'
$doSleep = $legList -contains 'sleep'
if ($legList -notcontains 'load') { $Load = 'none' }

$state = @{ cycle = 0; sleep_done = $false; lock_ok = $true; resizes = 0; locks = 0 }
if ($Resume -and (Test-Path $statePath)) {
  $j = Get-Content $statePath -Raw | ConvertFrom-Json
  foreach ($k in @($state.Keys)) { if ($null -ne $j.$k) { $state[$k] = $j.$k } }
  Say "== g4-$Tag RESUME at cycle $($state.cycle) $(Get-Date -Format o)"
} else {
  "== g4-$Tag start $(Get-Date -Format o) vendor=$Vendor minutes=$Minutes reconnects=$Reconnects legs=$Legs" | Set-Content $log
  Remove-Item $statusPath -ErrorAction SilentlyContinue
}

foreach ($p in $client, $envPath, $hostLog) {
  if (-not (Test-Path $p)) { Say "PREFLIGHT-FAILED missing $p"; exit 1 }
}
$token = Get-MgmtToken
if ((Get-Mgmt '/api/v1/status' $token).http -ne 200) { Say 'PREFLIGHT-FAILED mgmt api not answering'; exit 1 }
Say "preflight ok host=$((Get-Item 'C:\Program Files\punktfunk\punktfunk-host.exe').LastWriteTime) free_gb=$([int]((Get-PSDrive C).Free/1GB))"

# The client profile catalog gives each reconnect its mode; the resize leg drives the window.
if (Test-Path $profiles) { Copy-Item $profiles $profilesBak -Force } else { '{"version":1,"profiles":[]}' | Set-Content $profiles }
@{
  version  = 1
  profiles = @(
    @{ id = $modeA.id; name = 'g4 a'; overrides = @{ width = $modeA.w; height = $modeA.h; refresh_hz = $modeA.hz } },
    @{ id = $modeB.id; name = 'g4 b'; overrides = @{ width = $modeB.w; height = $modeB.h; refresh_hz = $modeB.hz } }
  )
} | ConvertTo-Json -Depth 8 | Set-Content $profiles -Encoding utf8

$knobs = @{ PUNKTFUNK_IDD_DIAG = '1'; RUST_LOG = 'info,pf_capture=debug' }
if ($Vendor -eq 'amd') { $knobs['PUNKTFUNK_ENCODER'] = 'amf'; $knobs['PUNKTFUNK_RENDER_ADAPTER'] = 'Radeon' }
else { $knobs['PUNKTFUNK_ENCODER'] = 'nvenc'; $knobs['PUNKTFUNK_RENDER_ADAPTER'] = 'NVIDIA' }
Set-HostEnv $knobs

if ($doLock) {
  $sid = (Get-Process -Id $PID).SessionId
  $act = New-ScheduledTaskAction -Execute 'C:\Windows\System32\tscon.exe' -Argument "$sid /dest:console"
  $prn = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
  Unregister-ScheduledTask $unlockTask -Confirm:$false -ErrorAction SilentlyContinue
  Register-ScheduledTask -TaskName $unlockTask -Action $act -Principal $prn -Force | Out-Null
  Say "unlock task registered: $unlockTask -> tscon $sid /dest:console"
}

Save-Snapshot 'pre' $token

# Status poller: an own process, so a blocked main loop never hides a blocked control plane.
$poller = Start-Job -ScriptBlock {
  param($url, $token, $out)
  while ($true) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $http = 0; $body = ''
    try {
      $r = Invoke-WebRequest -Uri $url -Headers @{ Authorization = "Bearer $token" } -SkipCertificateCheck -TimeoutSec 5
      $http = [int]$r.StatusCode; $body = $r.Content
    } catch { $http = 0 }
    $sw.Stop()
    ('{{"t":"{0}","ms":{1},"http":{2},"body":{3}}}' -f (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ss'),
      [int]$sw.Elapsed.TotalMilliseconds, $http, $(if ($body) { $body } else { 'null' })) | Add-Content $out
    Start-Sleep 2
  }
} -ArgumentList 'https://127.0.0.1:47990/api/v1/status', $token, $statusPath

$cert = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2 'C:\ProgramData\punktfunk\cert.pem'
$fp = $cert.GetCertHashString('SHA256').ToLower()
# ~18 s per cycle goes on the bring-up wait and the inter-cycle pause; take it off the
# streaming window so 100 cycles still fit inside $Minutes of wall clock.
$cycleSecs = [Math]::Max(45, [int]($Minutes * 60 / [Math]::Max(1, $Reconnects)) - 18)
$resizeEvery = if ($doResize -and $Resizes -gt 0) { [Math]::Max(1, [int][Math]::Ceiling($Reconnects / $Resizes)) } else { 0 }
$lockEvery = if ($doLock -and $Locks -gt 0) { [Math]::Max(1, [int][Math]::Ceiling($Reconnects / $Locks)) } else { 0 }
$sleepAt = [int]($Reconnects * 0.6)
# section 5 wants both 4 h AND 100 reconnects, so the loop runs until both are met; the hard cap
# stops a run whose cycles are overrunning rather than letting it eat the day.
$end = (Get-Date).AddMinutes($Minutes)
$hardEnd = (Get-Date).AddMinutes($Minutes * 1.5)
Say "plan cycle_s=$cycleSecs resize_every=$resizeEvery lock_every=$lockEvery sleep_at_cycle=$sleepAt end=$($end.ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ss'))Z hard_end=$($hardEnd.ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ss'))Z"
Start-Load ([int]($Minutes * 90 + 600))

try {
  while ((Get-Date) -lt $hardEnd -and ((Get-Date) -lt $end -or $state.cycle -lt $Reconnects)) {
    $state.cycle++
    $n = $state.cycle
    $mode = if ($resizeEvery -gt 0 -and ($n % $resizeEvery) -eq 0) { $modeB } else { $modeA }
    $out = "$root\g4-$Tag-$n.out"; $errf = "$root\g4-$Tag-$n.err"
    Mark 'cycle-start' "n=$n profile=$($mode.id)"
    $p = Start-Process -FilePath $client -ArgumentList @('--connect', '127.0.0.1:9777', '--fp', $fp,
      '--connect-timeout', '60', '--profile', $mode.id) -PassThru -RedirectStandardOutput $out -RedirectStandardError $errf
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalSeconds -lt 40 -and (Get-Mgmt '/api/v1/status' $token).body -notmatch '"video_streaming"\s*:\s*true') { Start-Sleep 2 }
    # The client's own startup probe leaves a ~790 ms compose-silence hole per session; the
    # cutter needs this mark to keep that test artefact out of the gap tally.
    Mark 'cycle-streaming' "n=$n after_ms=$([int]$sw.Elapsed.TotalMilliseconds)"
    Move-Furmark
    $stop = (Get-Date).AddSeconds($cycleSecs)

    if ($resizeEvery -gt 0 -and ($n % $resizeEvery) -eq 0 -and $state.resizes -lt $Resizes) {
      Start-Sleep ([Math]::Max(10, [int]($cycleSecs / 4)))
      $h = Get-MainWindow $p.Id
      if ($h -ne [IntPtr]::Zero) {
        $target = if (($state.resizes % 2) -eq 0) { $modeA } else { $modeB }
        $state.resizes++
        Mark 'resize-begin' "n=$n i=$($state.resizes) w=$($target.w) h=$($target.h)"
        [PfG4.W]::SetWindowPos($h, [IntPtr]::Zero, 60, 60, $target.w, $target.h, $SWP) | Out-Null
        Start-Sleep 20
        $s = (Get-Mgmt '/api/v1/status' $token).body
        $live = if ($s -match '"width"\s*:\s*(\d+)\s*,\s*"height"\s*:\s*(\d+)') { "$($Matches[1])x$($Matches[2])" } else { 'unknown' }
        $rms = if ($s -match '"last_resize_ms"\s*:\s*(\d+)') { $Matches[1] } else { 'none' }
        Mark 'resize-end' "n=$n i=$($state.resizes) live=$live last_resize_ms=$rms"
      } else { Mark 'resize-skip' "n=$n reason=no-window" }
    }

    if ($lockEvery -gt 0 -and ($n % $lockEvery) -eq 0 -and $state.locks -lt $Locks -and $state.lock_ok) {
      $state.locks++
      $state.lock_ok = Invoke-LockCycle $state.locks $token
      if (-not $state.lock_ok) { Say 'LOCK-LEG-ABORTED - the console did not come back; remaining lock cycles are skipped' }
    }

    if ($doSleep -and -not $state.sleep_done -and $n -ge $sleepAt) {
      $state.sleep_done = $true
      Invoke-SleepCycle $token
    }

    while ((Get-Date) -lt $stop -and -not $p.HasExited) { Start-Sleep 5 }
    $early = $p.HasExited
    if (-not $early) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue }
    $stats = Get-Content $out -ErrorAction SilentlyContinue | Where-Object { $_ -like 'stats:*' }
    $last = $stats | Select-Object -Last 1
    $wl = Select-String -Path $hostLog -Pattern 'wudf_pid' | Select-Object -Last 1
    $h = ''
    if ($wl) {
      $c = $wl.Line -replace "\x1b\[[0-9;]*m", ""
      if ($c -match 'wudf_pid=(\d+)') {
        $wp = Get-Process -Id ([int]$Matches[1]) -ErrorAction SilentlyContinue
        if ($wp) { $h = "handles=$($wp.HandleCount) threads=$($wp.Threads.Count) ws_mb=$([int]($wp.WorkingSet64/1MB))" }
      }
    }
    Mark 'cycle-end' "n=$n early_exit=$early stats=$(($stats | Measure-Object).Count) $h"
    if ($last) { Say "  $($last.Substring(0, [Math]::Min(240, $last.Length)))" }
    Remove-Item $out, $errf -ErrorAction SilentlyContinue
    if ($poller.State -ne 'Running') { Say 'status poller died - criterion 5 has a hole here' }
    $state | ConvertTo-Json -Compress | Set-Content $statePath
    Start-Sleep 6
    Start-Load ([Math]::Max(300, [int](($hardEnd - (Get-Date)).TotalSeconds)))
  }
} finally {
  Stop-Job $poller -ErrorAction SilentlyContinue
  Remove-Job $poller -Force -ErrorAction SilentlyContinue
  Save-Snapshot 'post' $token
  Restore-Box
  Mark 'run-end' "cycles=$($state.cycle) resizes=$($state.resizes) locks=$($state.locks) sleep=$($state.sleep_done)"
  Say "== g4-$Tag done $(Get-Date -Format o)"
}
