# HDR x 4:4:4 verification matrix for the in-driver encoder (design/windows-video-plane-overhaul.md).
#
# Four NVENC/HEVC rows on one virtual display -- SDR 4:2:0 (the control), SDR 4:4:4, HDR 4:2:0,
# HDR 4:4:4 -- each reporting the input the driver chose, the chroma_444 its caps reply carried,
# and what ffmpeg says the decoded stream actually is. The HDR 4:4:4 row is the point: packed RGB
# in, chroma_444 true out, a stream ffmpeg agrees is 4:4:4. Anything else is the regression.
#
# Copy to C:\Users\Public\ and launch through kick-task.ps1 (a console-user task): the run sets
# the display's colour mode, and session 0 gets ACCESS_DENIED on every SetDisplayConfig.
# Reuses s5-prep.ps1 (checkout + probe driver + test binary + redeploy) and probe-run.ps1
# (service stop, live_encode_probe, Annex-B extraction, ffmpeg) -- this script only sequences
# them, sets the PF_PROBE_HDR and PF_PROBE_444 knobs, and judges the rows.
#
# No commas and no colons in any parameter value: kick-task's -ScriptArgs mangles both.

param(
  [string]$Ref = 'fix/vdisplay-hdr-444',
  [string]$Tag = 'hdr444',
  [int]$Frames = 120,
  [string]$Backend = 'nvenc',
  [string]$Codec = 'hevc',
  # Where the box keeps its launchers. Only a dry run against stubs ever overrides this.
  [string]$PublicDir = 'C:\Users\Public',
  [switch]$NoPrep
)

$ErrorActionPreference = 'Continue'
$pub = $PublicDir
$log = Join-Path $pub ("live-" + $Tag + ".log")
Remove-Item $log -ErrorAction SilentlyContinue

function Say($text) { $text | Tee-Object -FilePath $log -Append }
function Rule { Say ('-' * 78) }

Say ("=== live-hdr444 " + (Get-Date -Format 'yyyy-MM-dd HH.mm.ss') + " ref=" + $Ref + " frames=" + $Frames + " ===")

$probeRun = Join-Path $pub 'probe-run.ps1'
$s5prep   = Join-Path $pub 's5-prep.ps1'
$wudfkill = Join-Path $pub 'wudfkill.ps1'
foreach ($need in @($probeRun, $s5prep)) {
  if (-not (Test-Path $need)) { Say ("ABORT missing " + $need); exit 2 }
}

# ---- prep: checkout + probe driver + test binary + redeploy -------------------------------
if ($NoPrep) {
  Say 'prep skipped (-NoPrep)'
} else {
  Say ("=== prep (s5-prep.ps1 -Ref " + $Ref + ") ===")
  & $s5prep -Ref $Ref 2>&1 | Tee-Object -FilePath $log -Append
  Say ("EXIT(prep)=" + $LASTEXITCODE)
  # A redeploy does not take effect until the old WUDFHost dies -- it keeps the OLD mapped image.
  if (Test-Path $wudfkill) {
    Say '=== wudfkill (force a fresh WUDFHost onto the new dll) ==='
    & $wudfkill 2>&1 | Tee-Object -FilePath $log -Append
  } else {
    Say 'WARN wudfkill.ps1 absent -- if every row reports the old behaviour the driver did not swap'
  }
}
try {
  $wudf = Get-Process WUDFHost -ErrorAction SilentlyContinue |
    Where-Object { $_.Modules.ModuleName -contains 'pf_vdisplay.dll' }
  if ($wudf) {
    Say ("WUDFHost pid=" + $wudf.Id + " started=" + $wudf.StartTime.ToString('HH.mm.ss') +
         " -- older than the redeploy above means it still has the OLD dll mapped")
  } else {
    Say 'WUDFHost with pf_vdisplay.dll not loaded yet (it starts on the first open)'
  }
} catch {
  Say ('WUDFHost module read failed -- ' + $_.Exception.Message)
}

# ---- the matrix ---------------------------------------------------------------------------
# Expectations are the point of the run, so they are declared, not inferred.
$rows = @(
  @{ Name = 'sdr420'; Hdr = '0'; C444 = '0'; WantTag = 'Bgra+420';  WantPix = 'yuv420p'     }
  @{ Name = 'sdr444'; Hdr = '0'; C444 = '1'; WantTag = 'Bgra+444';  WantPix = 'yuv444p'     }
  @{ Name = 'hdr420'; Hdr = '1'; C444 = '0'; WantTag = 'P010+420';  WantPix = 'yuv420p10le' }
  @{ Name = 'hdr444'; Hdr = '1'; C444 = '1'; WantTag = 'Rgb10+444'; WantPix = 'yuv444p10le' }
)

$results = @()
foreach ($row in $rows) {
  Rule
  Say ("=== row " + $row.Name + " (PF_PROBE_HDR=" + $row.Hdr + " PF_PROBE_444=" + $row.C444 + ") ===")
  $env:PF_PROBE_HDR = $row.Hdr
  $env:PF_PROBE_444 = $row.C444
  $out = & $probeRun -Backend $Backend -Codec $Codec -InputMode default -Frames $Frames -Tag ($Tag + '-' + $row.Name) 2>&1 | Out-String
  $out | Add-Content $log

  # The test prints the display's colour mode it set; the driver's reply carries input+chroma.
  $colour = if ($out -match 'probe hdr-state\W+want=(\S+)\s+set_ok=(\S+)\s+enabled=(\S+)') {
    "want=$($Matches[1]) set_ok=$($Matches[2]) enabled=$($Matches[3])"
  } else { 'not reported' }
  $state = if ($out -match '\bstate=(\d+)') { [int]$Matches[1] } else { -1 }
  $tag   = if ($out -match '\bname=(\S+)')  { $Matches[1] }      else { '' }
  $aus   = if ($out -match '\baus=(\d+)')   { [int]$Matches[1] } else { 0 }
  # `pix_fmt=` first: a bare token could just as easily be an ffmpeg command line being echoed.
  $pix = if ($out -match 'pix_fmt=(\S+)') { $Matches[1] }
         elseif ($out -match '\b(yuv4[24][40]p(?:10le|12le)?|gbrp(?:10le)?)\b') { $Matches[1] }
         else { 'not reported' }

  $results += [pscustomobject]@{
    Row = $row.Name; Colour = $colour; State = $state; Tag = $tag
    Aus = $aus; Pix = $pix; WantTag = $row.WantTag; WantPix = $row.WantPix; C444 = $row.C444
  }
  Say ("row " + $row.Name + " -> state=" + $state + " aus=" + $aus + " tag=" + $tag + " pix=" + $pix)
  Say ("   colour " + $colour)
}

# ---- the report ---------------------------------------------------------------------------
Rule
Say '=== RESULT ==='
# Loud and first: a row the display would not take a colour mode for encoded nothing, and a
# reader skimming the table would otherwise take its blanks for a result.
$blocked = @($results | Where-Object { $_.Tag -eq 'fmt' })
if ($blocked.Count -gt 0) {
  Say ''
  Say ('!!! DISPLAY COLOUR MODE BLOCKED -- ' + (($blocked | ForEach-Object { $_.Row }) -join ' '))
  Say '!!! Those rows never encoded a frame. Treat them as absent, not as evidence.'
  Say '!!! The probe hdr-state lines above say whether the display took the mode at all.'
  Say ''
}
Say ('{0,-8} {1,-11} {2,-11} {3,-13} {4,-13} {5}' -f 'row', 'input+caps', 'expected', 'stream', 'expected', 'verdict')

$fails = 0
foreach ($r in $results) {
  $why = @()
  # A row whose surface format did not match its input never encoded anything comparable.
  if ($r.Tag -eq 'fmt') {
    $why += 'DISPLAY COLOUR MODE WRONG -- the desktop did not present the surface this input reads'
  } elseif ($r.State -ne 3) {
    $why += ('probe did not finish (state=' + $r.State + ' tag=' + $r.Tag + ')')
  } else {
    $capsIs444 = $r.Tag -like '*+444'
    $inputIsFullChroma = ($r.Tag -like 'Bgra*') -or ($r.Tag -like 'Rgb10*') -or ($r.Tag -like 'Planar*')
    # Caps that advertise 4:4:4 over a subsampled input are the regression this matrix guards.
    if ($capsIs444 -and -not $inputIsFullChroma) {
      $why += 'REGRESSION -- caps advertise 4:4:4 over a subsampled input'
    }
    if ($r.Tag -ne $r.WantTag) { $why += ('input+caps is ' + $r.Tag + ' expected ' + $r.WantTag) }
    if ($r.Pix -eq 'not reported') {
      $why += 'ffmpeg reported no pixel format -- read the row output above'
    } elseif ($r.Pix -ne $r.WantPix) {
      $why += ('stream is ' + $r.Pix + ' expected ' + $r.WantPix)
      if ($capsIs444) { $why += 'REGRESSION -- the reply promised 4:4:4 the stream does not carry' }
    }
    if ($r.Aus -lt 2) { $why += 'fewer than two access units -- the numbers above mean nothing' }
  }
  $verdict = if ($why.Count -eq 0) { 'PASS' } else { 'FAIL ' + ($why -join ' / ') }
  if ($why.Count -ne 0) { $fails++ }
  Say ('{0,-8} {1,-11} {2,-11} {3,-13} {4,-13} {5}' -f $r.Row, $r.Tag, $r.WantTag, $r.Pix, $r.WantPix, $verdict)
}

Rule
$hdr444 = $results | Where-Object { $_.Row -eq 'hdr444' }
if ($hdr444 -and $hdr444.Tag -eq 'Rgb10+444' -and $hdr444.Pix -eq 'yuv444p10le') {
  Say 'HDR x 4:4:4 VERIFIED -- packed RGB in, chroma_444 out, a 4:4:4 10-bit stream on the wire'
} else {
  Say 'HDR x 4:4:4 NOT VERIFIED -- read the hdr444 row above'
}
Say ("rows failing: " + $fails + " of " + $results.Count)
Say ("log " + $log)
Say 'ALLDONE'
