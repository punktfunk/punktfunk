#requires -Version 7
<#
.SYNOPSIS
  Spike S6 (design/windows-video-plane-overhaul.md §3): pool vs pool-bypass, back to back, judged
  on the virtual display's compose cadence under a FurMark load.

.DESCRIPTION
  READ THIS BEFORE ACTING ON THE NUMBER. S6 measures cadence, but cadence is not the whole
  decision. §2.5 row 1 promises that a blocked encoder leaves the drain worker unaffected, and G3
  requires a hard-blocked encoder to cost one IDR and never a compose hitch. The bypass holds the
  next acquire on the encoder, bounded at 100 ms, so a blocked encoder does hitch this head.
  AMF, QSV and NVENC take the bypass by default (zero_copy in pf-driver-proto) because the copy
  runs on the 3D engine a game renders on; where it is not the default, a cadence PASS here is
  one input, next to that fault model.

  ONE driver binary, deployed before this runs. The legs differ only by the machine knob
  PFVD_POOL_BYPASS, which the driver reads at every encoder open - each leg still cycles the
  adapter (reset-pf-vdisplay.ps1), so both start on a fresh WUDFHost. The knob goes in the
  machine Environment key, which the driver's `knob()` reads live; host.env cannot reach it,
  because WUDFHost is not the host process.

    leg A  knob 0  -> the fused pass into a pool slot
    leg B  knob 1  -> the encoder reads the acquired surface, the next acquire on its completion

  The instrument is the driver's own "[pf-vd] cadence:" line - PresentDisplayQPCTime deltas in
  eighth frame periods, stamped after FinishedProcessingFrame in BOTH modes. The driver emits
  it in every build; this script reads it from the file log, which needs PFVD_DEBUG_LOG (the
  script sets it). §5-4's bar is "never exceed 2 frame periods"; that is the >=2.00 column.
  Encode latency and fps come from the client's own "stats:" lines.

  FurMark is the load AND the damage source: its window is parked on the VIRTUAL display and
  re-parked every 15 s. Without that the streamed desktop composes nothing, DWM presents nothing,
  and both legs measure an idle desktop. -NoLoad skips it and says so.

  Bypass only engages on a composed input kind (EncodeInput::composed: BGRA, or FP16 for AMF
  under HDR) at the surface's own size. A session that opens a converter kind (NV12, P010,
  packed 10-bit RGB) takes the pass - the leg B header then says mode=pool and the run is
  VOID. The report says so rather than comparing two identical legs. A frame with a blended
  pointer takes the pass, whatever the header says.

  Run ELEVATED, on the box, from a detached console-user task (kick-task.ps1): FurMark and the
  client both need the console session. Arguments carry no commas and no colons.

.EXAMPLE
  C:\Users\Public\kick-task.ps1 -Name s6 -Script C:\Users\Public\s6-cadence-ab.ps1 -ScriptArgs "-Tag s6 -Minutes 10"
#>
[CmdletBinding()]
param(
    [string]$Tag = 's6',
    [int]$Minutes = 10,
    # The session's mode. Also picks the virtual display to park FurMark on, and FurMark's own
    # render size.
    [int]$Width = 4096,
    [int]$Height = 2160,
    [int]$Fps = 120,
    # Client fingerprint; empty = the sha256 of the host's own cert, which is what a loopback
    # client pins.
    [string]$Fp = '',
    [switch]$NoLoad,
    [int]$GpuIndex = 0,
    [string]$Repo = 'C:\Users\Public\pf-phase0',
    [string]$Client = 'C:\Users\Public\punktfunk-native\target\release\punktfunk-session.exe',
    [string]$FurMark = 'C:\Program Files\Geeks3D\FurMark2_x64\furmark.exe',
    [string]$Service = 'PunktfunkHost',
    [string]$OutDir = 'C:\Users\Public'
)
$ErrorActionPreference = 'Continue'
$ProgressPreference = 'SilentlyContinue'
$PSNativeCommandUseErrorActionPreference = $false

$Log = Join-Path $OutDir "live-$Tag.log"
$MachineEnv = 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Environment'

function Say([string]$m) {
    $line = '{0}  {1}' -f (Get-Date -Format 'HH:mm:ss.fff'), $m
    Write-Host $line
    Add-Content -Path $Log -Value $line
}

"=== S6 pool vs bypass $(Get-Date -Format o) ===" | Set-Content $Log
$Reset = @(
    (Join-Path $Repo 'packaging\windows\reset-pf-vdisplay.ps1')
    (Join-Path $OutDir 'reset-pf-vdisplay.ps1')
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not (Test-Path $Client)) { Say "FATAL no client at $Client"; exit 1 }
if (-not $Reset) { Say "FATAL no reset-pf-vdisplay.ps1 under $Repo or $OutDir"; exit 1 }
Say "reset script: $Reset"
if (-not $Fp) {
    $cert = Join-Path $env:ProgramData 'punktfunk\cert.pem'
    if (-not (Test-Path $cert)) { Say "FATAL no cert at $cert and no -Fp"; exit 1 }
    $Fp = (Get-FileHash $cert -Algorithm SHA256).Hash.ToLower()
}

# --- the load, and where it has to live -------------------------------------------------------
Add-Type -AssemblyName System.Windows.Forms
Add-Type -Namespace S6 -Name Win -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool SetWindowPos(
    IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
[DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
'@

# The virtual display is the screen at the session's mode; failing that, any non-primary one. If
# neither exists the desktop has no virtual head and both legs would measure the physical panel.
function Get-VirtualBounds {
    $screens = [System.Windows.Forms.Screen]::AllScreens
    $byMode = $screens | Where-Object { $_.Bounds.Width -eq $Width -and $_.Bounds.Height -eq $Height }
    $pick = if ($byMode) { $byMode[0] } else { $screens | Where-Object { -not $_.Primary } | Select-Object -First 1 }
    if (-not $pick) { return $null }
    Say "virtual display: $($pick.DeviceName) $($pick.Bounds)"
    $pick.Bounds
}

# FurMark recreates its window when the demo starts, so parking once is not enough.
function Move-ToVirtual($proc, $rect) {
    if (-not $proc -or -not $rect) { return }
    try { $proc.Refresh() } catch { return }
    $h = $proc.MainWindowHandle
    if ($h -eq [IntPtr]::Zero) { return }
    [S6.Win]::SetWindowPos($h, [IntPtr]::Zero, $rect.X, $rect.Y, $rect.Width, $rect.Height, 0x0040) | Out-Null
    [S6.Win]::SetForegroundWindow($h) | Out-Null
}

function Start-Load([int]$seconds) {
    if ($NoLoad) { Say 'WARN -NoLoad: an idle GPU shows no contention and composes nothing'; return $null }
    if (-not (Test-Path $FurMark)) { Say "WARN no FurMark at $FurMark - running with NO load"; return $null }
    $a = @(
        '--demo', 'furmark-vk', '--width', "$Width", '--height', "$Height",
        '--max-time', "$seconds", '--no-score-box', '--disable-demo-options',
        '--gpu-index', "$GpuIndex"
    )
    Say "FurMark: $a"
    Start-Process -FilePath $FurMark -PassThru -ArgumentList $a
}

# --- leg plumbing ------------------------------------------------------------------------------
# WUDFHost runs as LOCAL SERVICE, so its temp dir - not C:\Windows\Temp - holds the log.
function Get-DriverLog {
    @(
        'C:\Windows\ServiceProfiles\LocalService\AppData\Local\Temp\pfvd-driver.log',
        'C:\Windows\Temp\pfvd-driver.log'
    ) | Where-Object { Test-Path $_ } | Sort-Object { (Get-Item $_).LastWriteTime } | Select-Object -Last 1
}

function Set-Knob([string]$name, [string]$value) {
    if ($value) { Set-ItemProperty -Path $MachineEnv -Name $name -Value $value }
    else { Remove-ItemProperty -Path $MachineEnv -Name $name -ErrorAction SilentlyContinue }
}

function Invoke-Leg([string]$name, [bool]$bypass) {
    Say "--- leg $name (bypass=$bypass) ---"
    Set-Knob 'PFVD_POOL_BYPASS' $(if ($bypass) { '1' } else { '0' })
    # The adapter cycle mints a fresh WUDFHost: file_log_enabled() resolves once per process.
    & pwsh -NoProfile -File $Reset *>&1 | Add-Content $Log
    Start-Service $Service -ErrorAction SilentlyContinue
    Start-Sleep 8
    $drv = Get-DriverLog
    $mark = if ($drv) { (Get-Item $drv).Length } else { 0 }
    Say "driver log $drv at $mark bytes"
    $seconds = $Minutes * 60
    $rect = Get-VirtualBounds
    if (-not $rect) { Say 'WARN no virtual display found - the load cannot be parked on it' }
    $load = Start-Load ($seconds + 60)
    Start-Sleep 10
    $out = Join-Path $OutDir "s6-$Tag-$name.out"
    $cli = Start-Process $Client -PassThru -WindowStyle Minimized `
        -RedirectStandardOutput $out -RedirectStandardError "$out.err" `
        -ArgumentList '--connect', '127.0.0.1:9777', '--fp', $Fp, '--stats'
    Say "client pid $($cli.Id) for $Minutes min"
    $stopAt = (Get-Date).AddSeconds($seconds)
    while ((Get-Date) -lt $stopAt) {
        Move-ToVirtual $load $rect
        Start-Sleep 15
    }
    Stop-Process -Id $cli.Id -Force -ErrorAction SilentlyContinue
    if ($load) { Stop-Process -Id $load.Id -Force -ErrorAction SilentlyContinue }
    Get-Process -Name 'furmark' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep 3
    $tail = if ($drv -and (Test-Path $drv)) {
        $fs = [IO.File]::Open($drv, 'Open', 'Read', 'ReadWrite')
        $fs.Seek($mark, 'Begin') | Out-Null
        $sr = New-Object IO.StreamReader($fs)
        $t = $sr.ReadToEnd(); $sr.Close(); $fs.Close(); $t -split "`r?`n"
    }
    else { @() }
    $cut = Join-Path $OutDir "s6-$Tag-$name.drv"
    $tail -join "`n" | Set-Content $cut
    Say "driver log cut -> $cut ($($tail.Count) lines)"
    [pscustomobject]@{ Name = $name; Drv = $tail; Out = $out }
}

# --- the report ------------------------------------------------------------------------------
# One leg's cadence lines folded into a single histogram. Bucket i covers [i/8, (i+1)/8) frame
# periods; bucket 23 is everything at or over 2.875, so buckets 16..23 are the gate's column -
# read as "at or over 2.00 periods", one bucket conservative against §5-4's "never exceed 2".
$BUCKETS = 24
function Get-Cadence($lines) {
    $b = New-Object 'long[]' $BUCKETS
    $n = 0L; $max = 0L; $wins = 0; $fps = 0
    foreach ($l in $lines) {
        if ($l -notmatch 'cadence: mode=(\w+) fps=(\d+) win_ms=(\d+) n=(\d+) max_us=(\d+) h=([\d/]+)') { continue }
        $wins++
        $fps = [int]$Matches[2]
        $n += [long]$Matches[4]
        $max = [Math]::Max($max, [long]$Matches[5])
        $h = $Matches[6] -split '/'
        for ($i = 0; $i -lt $BUCKETS -and $i -lt $h.Count; $i++) { $b[$i] += [long]$h[$i] }
    }
    [pscustomobject]@{ Windows = $wins; N = $n; MaxUs = $max; Buckets = $b; Fps = $fps }
}

# Upper edge, in frame periods, of the bucket holding the q-quantile. A bucket answer, not an
# interpolation - the histogram is the only sample the driver keeps.
function Get-EdgePeriods($c, [double]$q) {
    if ($c.N -eq 0) { return 0.0 }
    $want = [Math]::Max(1, [Math]::Ceiling($q * $c.N))
    $seen = 0L
    for ($i = 0; $i -lt $BUCKETS; $i++) {
        $seen += $c.Buckets[$i]
        if ($seen -ge $want) { return ($i + 1) / 8.0 }
    }
    $BUCKETS / 8.0
}

function Get-Mode($lines) {
    $m = $lines | Select-String -Pattern 'encode: backend \d+ open (\d+)x(\d+) (\w+).* mode=(\w+)' |
        Select-Object -Last 1
    if ($m) {
        $g = $m.Matches[0].Groups
        "$($g[1].Value)x$($g[2].Value) $($g[3].Value) mode=$($g[4].Value)"
    }
    else { 'no encoder-open line' }
}

function Get-Mid($v, [double]$q) {
    if (-not $v -or $v.Count -eq 0) { return 0.0 }
    $s = $v | Sort-Object
    [double]$s[[Math]::Min($s.Count - 1, [int][Math]::Floor($q * $s.Count))]
}

function Get-Client($outPath) {
    $fps = @(); $e2e = @(); $enc = @()
    if (Test-Path $outPath) {
        foreach ($l in Get-Content $outPath) {
            if ($l -notmatch '^stats: ') { continue }
            if ($l -match '(\d+) fps') { $fps += [double]$Matches[1] }
            if ($l -match 'e2e ([\d.]+)/([\d.]+) ms') { $e2e += [double]$Matches[1] }
            if ($l -match 'encode ([\d.]+)') { $enc += [double]$Matches[1] }
        }
    }
    [pscustomobject]@{ Fps = (Get-Mid $fps 0.5); E2e = (Get-Mid $e2e 0.5); Encode = (Get-Mid $enc 0.5); Samples = $fps.Count }
}

function Show-Leg($leg) {
    $c = Get-Cadence $leg.Drv
    $s = Get-Client $leg.Out
    $le1 = ($c.Buckets[0..7] | Measure-Object -Sum).Sum
    $ge2 = ($c.Buckets[16..($BUCKETS - 1)] | Measure-Object -Sum).Sum
    $pct = { param($x) if ($c.N -gt 0) { '{0:N3}%' -f (100.0 * $x / $c.N) } else { 'n/a' } }
    $period = 1000000.0 / [Math]::Max(1, $(if ($c.Fps -gt 0) { $c.Fps } else { $Fps }))
    Say ''
    Say "leg $($leg.Name): $(Get-Mode $leg.Drv)"
    Say "  cadence  windows=$($c.Windows) frames=$($c.N) session_fps=$($c.Fps)"
    Say "  <1.00 period $(& $pct $le1)    >=2.00 periods $ge2 $(& $pct $ge2)"
    Say ("  periods  p50<={0:N3}  p95<={1:N3}  p99<={2:N3}  max {3:N3} ({4} us)" -f `
        (Get-EdgePeriods $c 0.5), (Get-EdgePeriods $c 0.95), (Get-EdgePeriods $c 0.99), `
        ($c.MaxUs / $period), $c.MaxUs)
    Say "  histogram (eighth periods 0.000..2.875+) $($c.Buckets -join '/')"
    Say "  client   fps $($s.Fps)  e2e p50 $($s.E2e) ms  host encode $($s.Encode) ms  windows $($s.Samples)"
    [pscustomobject]@{ Name = $leg.Name; N = $c.N; Ge2 = $ge2; MaxUs = $c.MaxUs; Fps = $s.Fps; Mode = (Get-Mode $leg.Drv) }
}

Set-Knob 'PFVD_DEBUG_LOG' '1'
$a = Invoke-Leg 'pool' $false
$b = Invoke-Leg 'bypass' $true
Set-Knob 'PFVD_POOL_BYPASS' ''

Say ''
Say '================ S6 report ================'
$ra = Show-Leg $a
$rb = Show-Leg $b
Say ''
if ($rb.Mode -notmatch 'mode=bypass') {
    Say 'VOID: leg bypass ran on the pool. The session opened a kind the bypass cannot'
    Say '      take (P010 or planar). Re-run SDR 8-bit.'
}
elseif ($ra.N -eq 0 -or $rb.N -eq 0) {
    Say 'VOID: a leg produced no cadence samples - check PFVD_DEBUG_LOG and the driver log path'
}
else {
    $an = 10000.0 * $ra.Ge2 / $ra.N
    $bn = 10000.0 * $rb.Ge2 / $rb.N
    Say ("frames composed                    pool {0}  bypass {1}" -f $ra.N, $rb.N)
    Say ("2-period-or-worse per 10k frames    pool {0:N2}  bypass {1:N2}" -f $an, $bn)
    Say ("worst delta us                     pool {0}  bypass {1}" -f $ra.MaxUs, $rb.MaxUs)
    Say ("client fps                         pool {0}  bypass {1}" -f $ra.Fps, $rb.Fps)
    if ($bn -le $an -and $rb.MaxUs -le ($ra.MaxUs * 1.1)) { Say 'S6 cadence PASS: no regression vs the pool' }
    else { Say 'S6 cadence FAIL: keep the pool (design §3 fail path); G3 is met at one pass' }
    Say 'Either way the fault-model objection in this header stands: read it before adopting.'
}
Say 'DONE'
