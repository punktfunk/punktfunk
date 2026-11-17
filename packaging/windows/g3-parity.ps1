#requires -Version 7.0
<#
.SYNOPSIS
  G3 HDR + 4:4:4 parity battery for the post-cutover build, against the pre-cutover
  reference recorded in design/windows-video-plane-overhaul.md section 10.

.DESCRIPTION
  Two legs, one log, no session and no display write:

    1. HDR P010 converter. `punktfunk-host hdr-p010-selftest` at 1920x1080 and
       2560x1440 drives pf_encode_win::convert::HdrP010Converter - the same type
       the driver's P010 pool slot builds. The eight per-colour 10-bit codes are
       compared here against the pre-cutover table, so the log says MATCH or DIFF
       per colour instead of leaving a human to diff two screenfuls.

    2. NVENC 4:4:4 encoder leg. The ignored `nvenc_444_on_glass_probe` writes a
       FREXT 4:4:4 and a 4:2:0 HEVC stream from the same pattern. Any stream
       already sitting at the probe's fixed output path is preserved as the `pre`
       artefact before the run, and hashes are compared when one exists.

  What this cannot cover is printed at the end: no instrument reaches the driver's
  own 4:4:4 or 10-bit input modes. Read that section - it is the honest bound on
  the result.

.PARAMETER Tag     Log suffix. Writes C:\Users\Public\live-<Tag>.log. Default g3.
.PARAMETER Vendor  Adapter for the selftest: nvidia, amd, intel, or any to leave
                   the pin off. A PASS only ever speaks for the adapter it ran on.
                   Default nvidia.
.PARAMETER Stage   Directory holding the two staged binaries. Default C:\Users\Public\g3.
.PARAMETER SkipEncode  Run only the converter leg (no NVENC session opened).

.EXAMPLE
  pwsh -NoProfile -File C:\Users\Public\g3-parity.ps1 -Tag g3 -Vendor nvidia
#>
param(
    [string]$Tag = 'g3',
    [string]$Vendor = 'nvidia',
    [string]$Stage = 'C:\Users\Public\g3',
    [switch]$SkipEncode
)

$ErrorActionPreference = 'Continue'
$stage = $Stage
$hostExe = Join-Path $stage 'punktfunk-host.exe'
$testExe = Join-Path $stage 'pf_encode_win-tests.exe'
$outDir = 'C:\Users\Public\parity-post'
$probeDir = 'C:\Users\Public'
$log = Join-Path 'C:\Users\Public' "live-$Tag.log"

# Each directory that exists is prepended, so an installed host is searched first, then
# FFmpeg, then the stage. ffprobe is a test tool here, not a dependency of the host.
foreach ($d in @($stage, 'C:\Users\Public\ffmpeg\bin', 'C:\Program Files\Punktfunk')) {
    if (Test-Path $d) { $env:PATH = "$d;$env:PATH" }
}

function Say([string]$m) { Write-Output $m; Add-Content -Path $log -Value $m }
function Head([string]$m) { Say ''; Say ('=' * 72); Say $m; Say ('=' * 72) }

# Pre-cutover reference: the codes the selftest printed on the ring build at both
# sizes. Order matches the selftest's own bar order.
$ref = [ordered]@{
    'red1.0'   = @(325, 448, 598)
    'green0.5' = @(394, 439, 478)
    'blue4.0'  = @(305, 674, 542)
    'white1.0' = @(489, 512, 512)
    'black'    = @(64, 512, 512)
    'gray0.5'  = @(431, 512, 512)
    'white4.0' = @(614, 512, 512)
    'amber2.0' = @(493, 419, 537)
}
# Kept as strings: the selftest prints a '.' decimal, and under a comma-decimal locale
# [double]::Parse('1.03') is 103 and a double prints as 1,03.
$refErr = @('1.03', '0.82', '0.75')

Set-Content -Path $log -Value "=== g3-parity $Tag $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss') ==="
Say "host exe : $hostExe"
Say "test exe : $testExe"
if (Test-Path (Join-Path $stage 'SHA.txt')) { Say "build sha: $(Get-Content (Join-Path $stage 'SHA.txt'))" }
New-Item -ItemType Directory -Force -Path $outDir, (Join-Path $outDir 'pre'), (Join-Path $outDir 'post') | Out-Null

$verdicts = [System.Collections.Generic.List[string]]::new()

# ---------------------------------------------------------------- leg 1: HDR
Head 'LEG 1 - HDR P010 converter (pf_encode_win::convert::HdrP010Converter)'
if (-not (Test-Path $hostExe)) {
    Say "SKIP - $hostExe is not staged"
    $verdicts.Add('hdr-p010: SKIP (no host exe)')
}
else {
    foreach ($size in @('1920x1080', '2560x1440')) {
        Say ''
        Say "--- hdr-p010-selftest $size $Vendor ---"
        # `any` leaves the adapter arg off, which takes the default hardware device.
        $raw = if ($Vendor -eq 'any') { & $hostExe hdr-p010-selftest $size 2>&1 }
        else { & $hostExe hdr-p010-selftest $size $Vendor 2>&1 }
        $code = $LASTEXITCODE
        $raw | ForEach-Object { Say "  | $_" }

        $diffs = [System.Collections.Generic.List[string]]::new()
        $seen = 0
        foreach ($line in $raw) {
            $m = [regex]::Match([string]$line, '^\s+(\S+)\s+[\d.]+/(\d+)\s+[\d.]+/(\d+)\s+[\d.]+/(\d+)')
            if (-not $m.Success) { continue }
            $name = $m.Groups[1].Value
            if (-not $ref.Contains($name)) { continue }
            $seen++
            $got = @([int]$m.Groups[2].Value, [int]$m.Groups[3].Value, [int]$m.Groups[4].Value)
            $want = $ref[$name]
            if ($got[0] -eq $want[0] -and $got[1] -eq $want[1] -and $got[2] -eq $want[2]) {
                Say ("  MATCH  {0,-10} {1}/{2}/{3}" -f $name, $got[0], $got[1], $got[2])
            }
            else {
                $d = "{0} got {1}/{2}/{3} want {4}/{5}/{6}" -f $name, $got[0], $got[1], $got[2], $want[0], $want[1], $want[2]
                Say "  DIFF   $d"
                $diffs.Add($d)
            }
        }

        $me = [regex]::Match(($raw -join "`n"), 'max abs error:\s+Y=([\d.]+).*?Cb=([\d.]+).*?Cr=([\d.]+)')
        if ($me.Success) {
            $e = @($me.Groups[1].Value, $me.Groups[2].Value, $me.Groups[3].Value)
            $same = ($e[0] -eq $refErr[0]) -and ($e[1] -eq $refErr[1]) -and ($e[2] -eq $refErr[2])
            Say ("  max abs error {0}/{1}/{2} vs pre {3}/{4}/{5} - {6}" -f $e[0], $e[1], $e[2], $refErr[0], $refErr[1], $refErr[2], $(if ($same) { 'MATCH' } else { 'DIFF' }))
            if (-not $same) { $diffs.Add('max abs error') }
        }
        else { $diffs.Add('max abs error line not found') }

        if ($seen -ne $ref.Count) { $diffs.Add("parsed $seen of $($ref.Count) colour rows") }

        if ($code -eq 0 -and $diffs.Count -eq 0) {
            Say "  VERDICT $size : PARITY - every colour code and every error bound reproduces"
            $verdicts.Add("hdr-p010 $size : PARITY")
        }
        else {
            Say "  VERDICT $size : REGRESSION (exit $code, $($diffs.Count) difference(s))"
            $diffs | ForEach-Object { Say "    - $_" }
            $verdicts.Add("hdr-p010 $size : REGRESSION")
        }
    }
}

# ------------------------------------------------------------- leg 2: 4:4:4
Head 'LEG 2 - NVENC 4:4:4 encoder leg (nvenc_444_on_glass_probe)'
if ($SkipEncode) {
    Say 'SKIP - SkipEncode was passed'
    $verdicts.Add('nvenc-444: SKIP (asked)')
}
elseif (-not (Test-Path $testExe)) {
    Say "SKIP - $testExe is not staged"
    $verdicts.Add('nvenc-444: SKIP (no test exe)')
}
else {
    $streams = @('nvenc444_probe.h265', 'nvenc420_probe.h265')

    # The probe writes to fixed paths. Anything already there predates this run;
    # keep it once as the `pre` artefact and never overwrite that copy.
    foreach ($s in $streams) {
        $live = Join-Path $probeDir $s
        $keep = Join-Path $outDir "pre\$s"
        if ((Test-Path $live) -and -not (Test-Path $keep)) {
            Copy-Item $live $keep -Force
            Say "preserved pre-existing $s as parity-post\pre\$s ($((Get-Item $keep).Length) bytes)"
        }
    }

    Say ''
    Say '--- running the probe ---'
    $raw = & $testExe nvenc_444_on_glass_probe --ignored --nocapture --test-threads 1 2>&1
    $code = $LASTEXITCODE
    $raw | ForEach-Object { Say "  | $_" }

    $ffprobe = 'C:\Users\Public\ffmpeg\bin\ffprobe.exe'
    foreach ($s in $streams) {
        $live = Join-Path $probeDir $s
        if (-not (Test-Path $live)) { Say "  MISSING $s - the probe produced no stream"; continue }
        $post = Join-Path $outDir "post\$s"
        Copy-Item $live $post -Force
        $h = (Get-FileHash $post -Algorithm SHA256).Hash
        Say ("  {0}  {1} bytes  sha256 {2}" -f $s, (Get-Item $post).Length, $h)
        $keep = Join-Path $outDir "pre\$s"
        if (Test-Path $keep) {
            $ph = (Get-FileHash $keep -Algorithm SHA256).Hash
            if ($ph -eq $h) { Say "    BYTE-IDENTICAL to the preserved pre artefact" }
            else { Say "    DIFFERS from the preserved pre artefact (pre sha256 $ph)" }
        }
        else { Say '    no pre artefact on this box - nothing to byte-compare against' }
        if (Test-Path $ffprobe) {
            $pf = & $ffprobe -v error -select_streams v:0 -show_entries stream=pix_fmt,profile,width,height -of default=nw=1 $post 2>&1
            $pf | ForEach-Object { Say "    ffprobe $_" }
        }
    }
    if ($code -eq 0) { $verdicts.Add('nvenc-444: probe ran') }
    else {
        Say "  the probe exited $code - read the block above for the NVENC error"
        $verdicts.Add("nvenc-444: FAILED (exit $code)")
    }
}

# ------------------------------------------------------------------ summary
Head 'SUMMARY'
$verdicts | ForEach-Object { Say "  $_" }

Head 'NOT COVERED BY THIS BATTERY'
Say '  Driver input modes. Both legs run in this process on a device of their own'
Say '  making. Neither opens the driver, so neither proves that the pool hands the'
Say '  encoder the same pixels the host used to. The live session covers that'
Say '  qualitatively only.'
Say ''
Say '  HDR with 4:4:4 has no driver input kind. encode/thread.rs spec_for maps any'
Say '  hdr request to InputKind::P010, which is 4:2:0, and NVENC only emits FREXT'
Say '  4:4:4 from packed RGB. The pre-cutover host fed Rgb10a2 here, so that'
Say '  combination really was full chroma. NVENC does catch it on the first frame'
Say '  and warns, but the SET_ENCODE reply carrying chroma_444 has already gone to'
Say '  the host by then, and the warning stays inside WUDFHost.'
Say ''
Say '  The other two ignored live tests are not parity instruments at all, whatever'
Say '  they run on. qsv_live_hevc10_hdr asserts only stream shape over 30 frames,'
Say '  and amf_hdr_encode_live_smoke feeds an uninitialised P010 texture at Yuv420.'
Say '  Neither is 4:4:4 and neither carries a colour reference to compare against.'
Say '  Intel hardware for the QSV leg does exist on VM 9200 at 192.168.1.49.'

Say ''
Say "log: $log"
