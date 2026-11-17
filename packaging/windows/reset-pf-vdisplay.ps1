#requires -Version 5.1
<#
.SYNOPSIS
  Recover the pf-vdisplay (punktfunk) virtual-display driver after it WEDGES under rapid ADD/REMOVE
  churn - no reboot. The dev-iteration counterpart to redeploy-pf-vdisplay.ps1.

.DESCRIPTION
  Ghost "Generic Monitor (Punktfunk)" device nodes each pin one of IddCx's ~16 monitor slots; once
  they pile up, IOCTL_ADD returns 0x80070490 ERROR_NOT_FOUND and every session fails to create a
  virtual output. A host restart's IOCTL_CLEAR_ALL departs only that host's live monitors; it
  removes no ghost node.

  Steps (run ELEVATED):
    1. Stop the host service (it holds the driver's control device).
    2. pnputil /remove-device the GHOST (Status != OK = not-present) punktfunk virtual-monitor nodes
       that accumulated - the root of the slot exhaustion.
    3. Disable + Enable the pf-vdisplay adapter (ROOT\DISPLAY\*, "Punktfunk Virtual Display") to
       reload the IddCx driver instance and reset its monitor list. (Restart-PnpDevice does NOT exist
       on this box's PowerShell, so we disable+enable explicitly.)
    4. Restart the host service.
  Avoids a reboot on purpose (this box boots to Proxmox).

.PARAMETER Service     Host service name. Default PunktfunkHost.
.PARAMETER AdapterName FriendlyName substring of the IddCx adapter to cycle. Default "Punktfunk
                       Virtual Display" (NOT SudoVDA's "SudoMaker Virtual Display Adapter").
.PARAMETER GhostMatch  FriendlyName substring of the virtual monitors to reap. Default "Punktfunk".
.PARAMETER KeepGhosts  Skip the ghost-node cleanup; only cycle the adapter.
.PARAMETER NoHost      Don't stop/start the host service (just reset the driver) - used by
                       redeploy-pf-vdisplay.ps1, which manages the service itself.
.PARAMETER Verify      After recovery, run a punktfunk-probe loopback and report whether ADD works
                       again (best-effort; needs punktfunk-probe.exe on PATH or via -Probe).
.PARAMETER Probe       Path to punktfunk-probe.exe for -Verify.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File reset-pf-vdisplay.ps1
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File reset-pf-vdisplay.ps1 -Verify -Probe C:\t-goal1\debug\punktfunk-probe.exe
#>
[CmdletBinding()]
param(
    [string]$Service     = 'PunktfunkHost',
    [string]$AdapterName = 'Punktfunk Virtual Display',
    [string]$GhostMatch  = 'Punktfunk',
    [switch]$KeepGhosts,
    [switch]$NoHost,
    [switch]$Verify,
    [string]$Probe
)
$ErrorActionPreference = 'Continue'

function Get-PfAdapter {
    Get-PnpDevice -Class Display -ErrorAction SilentlyContinue |
        Where-Object { $_.FriendlyName -match $AdapterName } | Select-Object -First 1
}

# 1) Stop the host so it isn't mid-IOCTL during the reset (it holds the control device).
$svc = Get-Service $Service -ErrorAction SilentlyContinue
$hostWasRunning = $svc -and $svc.Status -eq 'Running'
if (-not $NoHost -and $hostWasRunning) {
    Write-Host "==> stopping $Service"
    Stop-Service $Service -Force
    Start-Sleep -Seconds 2
}

# 2) Reap the ghost (not-present) punktfunk virtual-monitor device nodes.
if (-not $KeepGhosts) {
    $ghosts = Get-PnpDevice -Class Monitor -ErrorAction SilentlyContinue |
        Where-Object { $_.Status -ne 'OK' -and $_.FriendlyName -match $GhostMatch }
    Write-Host "==> removing $($ghosts.Count) ghost virtual-monitor node(s)"
    $removed = 0
    foreach ($g in $ghosts) {
        pnputil /remove-device $g.InstanceId *> $null
        if ($LASTEXITCODE -eq 0) { $removed++ }
    }
    Write-Host "    removed $removed"
}

# 3) Reload the IddCx adapter instance (disable + enable) to clear its monitor list.
$ad = Get-PfAdapter
if (-not $ad) {
    Write-Warning "pf-vdisplay adapter '$AdapterName' not found (Class Display) - is the driver installed?"
}
else {
    Write-Host "==> cycling adapter $($ad.InstanceId)"
    Disable-PnpDevice -InstanceId $ad.InstanceId -Confirm:$false -ErrorAction Continue
    Start-Sleep -Seconds 3
    Enable-PnpDevice -InstanceId $ad.InstanceId -Confirm:$false -ErrorAction Continue
    Start-Sleep -Seconds 3
    $st = (Get-PnpDevice -InstanceId $ad.InstanceId -ErrorAction SilentlyContinue).Status
    if ($st -ne 'OK') {
        # One retry - a disabled root device occasionally needs a second enable to come back OK.
        Enable-PnpDevice -InstanceId $ad.InstanceId -Confirm:$false -ErrorAction Continue
        Start-Sleep -Seconds 2
        $st = (Get-PnpDevice -InstanceId $ad.InstanceId -ErrorAction SilentlyContinue).Status
    }
    Write-Host "    adapter status: $st"
}

# 4) Restart the host.
if (-not $NoHost -and $svc) {
    Write-Host "==> starting $Service"
    Start-Service $Service
    Start-Sleep -Seconds 3
    Write-Host "    $Service status: $((Get-Service $Service -ErrorAction SilentlyContinue).Status)"
}

# 5) Optional: probe to confirm ADD recovers.
if ($Verify) {
    if (-not $Probe) {
        $Probe = (Get-Command punktfunk-probe.exe -ErrorAction SilentlyContinue).Source
    }
    if (-not $Probe -or -not (Test-Path $Probe)) {
        Write-Warning "-Verify: punktfunk-probe.exe not found (pass -Probe <path>); skipping verification."
    }
    else {
        $log = Join-Path $env:ProgramData 'punktfunk\logs\host.log'
        Write-Host "==> verifying with $Probe"
        & $Probe *> $null
        Start-Sleep -Seconds 2
        $last = Get-Content $log -Tail 80 -ErrorAction SilentlyContinue |
            Select-String -Pattern 'pf-vdisplay created|Element nicht|0x80070490' | Select-Object -Last 1
        if ($last -match 'created') { Write-Host "    OK: ADD succeeded after reset." }
        elseif ($last) { Write-Warning "    ADD still failing after reset: $($last.Line.Trim())" }
        else { Write-Warning "    no ADD outcome found in the log; check $log." }
    }
}

Write-Host "pf-vdisplay reset done."
