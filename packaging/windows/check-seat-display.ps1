<#
.SYNOPSIS
  Report whether a multi-seat host can actually give its seats a pf-vdisplay display.

.DESCRIPTION
  The seats add-on needs `pf_vdisplay_seats.inf` to win the hardware id `RdpIdd_IndirectDisplay`,
  and losing it is SILENT: the seat session still starts, on Microsoft's own remote display adapter,
  and only the missing stream says otherwise. So this asserts the thing that matters - WHICH driver
  bound each seat devnode - rather than that a session came up.

  Winning that id is not about the certificate. Inbox `rdpidd.inf` reports the same driver rank we
  do (0x00FF0000), and the tie goes to the newer DriverVer DATE; inbox is frozen at 06/21/2006, so
  any current build wins until a servicing update re-dates it. That is the failure this exists to
  catch (planning `windows-seat-display-tier.md` sec 5d).

  Exit 0 = every live seat devnode is ours. Exit 1 = at least one is not, or the package is absent.
  Seat devnodes only exist while a seat session is connected, so run this with one up.

.PARAMETER Quiet
  Print only the verdict line.
#>
[CmdletBinding()]
param([switch]$Quiet)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

function Say([string]$m) { if (-not $Quiet) { Write-Output $m } }

# --- 1. SKU ------------------------------------------------------------------------------------
# ProductType 1 = workstation, 2 = domain controller, 3 = server. Concurrent seats need a Server
# SKU plus RDS CALs; a client SKU serves ONE session, so the display can be perfect and the second
# seat still never arrives. Warn rather than fail: a single-seat client box is a legitimate setup.
$os = Get-CimInstance Win32_OperatingSystem
Say "OS        : $($os.Caption) (build $($os.BuildNumber), ProductType $($os.ProductType))"
if ($os.ProductType -eq 1) {
    Write-Warning ("client SKU: Windows serves one session at a time here, so concurrent seats " +
        "need Windows Server plus an RDS CAL per seat. See https://docs.punktfunk.unom.io/docs/developers/multi-seat-contract.")
}

# --- 2. is the seats package even installed? ----------------------------------------------------
$drivers = (& pnputil /enum-drivers) -join "`n"
$haveSeats = $drivers -match 'pf_vdisplay_seats\.inf'
Say "seats INF : $(if ($haveSeats) { 'installed' } else { 'not installed by that name' }) (informational - the verdict below is from real bindings)"

# --- 2b. the tie-break, checked BEFORE it costs anyone a seat -----------------------------------
# Equal rank goes to the newer DriverVer date, so a seats package stamped older than the inbox
# driver loses the id and nothing says so. Read inbox's date rather than hardcoding 06/21/2006:
# the whole point of this check is that Microsoft could move it.
$inboxInf = Join-Path $env:SystemRoot 'INF\rdpidd.inf'
if (Test-Path $inboxInf) {
    $inboxDate = ((Select-String -Path $inboxInf -Pattern '^\s*DriverVer\s*=' | Select-Object -First 1).Line -split '[=,]')[1].Trim()
    $ourDates = [regex]::Matches($drivers, 'pf_vdisplay(?:_seats)?\.inf[\s\S]{0,400}?Driver Version:\s*(\d{2}/\d{2}/\d{4})') |
        ForEach-Object { $_.Groups[1].Value }
    Say "inbox rdpidd DriverVer date : $inboxDate"
    foreach ($d in $ourDates) {
        Say "our package DriverVer date  : $d"
        if ([datetime]::Parse($d) -le [datetime]::Parse($inboxDate)) {
            Write-Warning ("our driver is dated $d, inbox rdpidd is $inboxDate - equal rank goes " +
                'to the NEWER date, so this package will lose RdpIdd_IndirectDisplay. Rebuild it.')
        }
    }
}

# --- 3. who owns each live seat devnode? --------------------------------------------------------
# `Status -eq OK` filters the stale nodes a finished session leaves behind: those keep their last
# binding and would report a stale verdict either way.
$seats = @(Get-PnpDevice -ErrorAction SilentlyContinue |
    Where-Object { $_.InstanceId -like 'SWD\REMOTEDISPLAYENUM*' -and $_.Status -eq 'OK' })

if (-not $seats) {
    Say 'seat nodes: none live'
    Write-Output 'INCONCLUSIVE: no seat session is connected - run this while a seat is up.'
    exit 1
}

$bad = @()
foreach ($d in $seats) {
    $inf = try { (Get-PnpDeviceProperty -InstanceId $d.InstanceId -KeyName 'DEVPKEY_Device_DriverInfPath' -EA Stop).Data }
           catch { '<unknown>' }
    $rank = try { (Get-PnpDeviceProperty -InstanceId $d.InstanceId -KeyName 'DEVPKEY_Device_DriverRank' -EA Stop).Data }
            catch { $null }
    $ours = $d.FriendlyName -like '*Punktfunk*'
    if (-not $ours) { $bad += $d }
    $rankHex = if ($null -ne $rank) { '0x{0:X8}' -f [uint32]$rank } else { 'n/a' }
    Say ("  {0}`n    driver={1} inf={2} rank={3} {4}" -f
        $d.InstanceId, $d.FriendlyName, $inf, $rankHex, $(if ($ours) { 'OK' } else { 'NOT OURS' }))
}

if ($bad) {
    Write-Output ("FAIL: $($bad.Count) of $($seats.Count) seat display(s) are not pf-vdisplay. " +
        'The seats package lost `RdpIdd_IndirectDisplay` - check its DriverVer date against ' +
        "the inbox rdpidd.inf, which the tie is decided on.")
    exit 1
}

Write-Output "OK: $($seats.Count) seat display(s), all pf-vdisplay."
exit 0
