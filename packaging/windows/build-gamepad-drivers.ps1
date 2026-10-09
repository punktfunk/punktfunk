<#
.SYNOPSIS
  Build + sign the punktfunk virtual-gamepad UMDF drivers (pf-gamepad = DualSense/DualShock 4/Edge/Deck, pf-xusb =
  Xbox 360 / XInput) FROM SOURCE, in CI, and stage them for the host installer. The gamepad analogue of
  build-pf-vdisplay.ps1 - replaces the checked-in prebuilt binaries (packaging/windows/gamepad-drivers/)
  so the .dll/.inf/.cat stay in lockstep with the source and never go stale.

.DESCRIPTION
  Both drivers are members of the in-tree drivers workspace (packaging/windows/drivers/), so one
  `cargo build --release` builds the whole workspace (this shares wdk-sys/wdk-build + the bindgen pin with
  pf-vdisplay). Then, per driver: CLEAR the FORCE_INTEGRITY PE bit, sign the .dll, stampinf a DriverVer
  into the INF; then Inf2Cat both catalogs and sign them. Both drivers share ONE self-signed cert (or a
  supplied DRIVER_CERT secret) + ONE exported .cer - the layout `punktfunk-host.exe driver install
  --gamepad` consumes (per-driver .inf/.cat/.dll + one shared punktfunk-driver.cer).

  Output (-Out): pf_gamepad.{dll,inf,cat} + pf_xusb.{dll,inf,cat} + pf_mouse.{dll,inf,cat} +
  punktfunk-driver.cer. (pf_mouse is the resident virtual HID pointer, not a gamepad - it shares
  this pipeline + the --gamepad install path.)

.EXAMPLE
  pwsh -File build-gamepad-drivers.ps1 -Out C:\t\gamepad
#>
[CmdletBinding()]
param(
    [string]$DriversDir = (Join-Path $PSScriptRoot 'drivers'),
    [Parameter(Mandatory = $true)][string]$Out,
    [string]$DriverVer,
    [string]$CertPfxB64 = $env:DRIVER_CERT_PFX_B64,
    [string]$CertPassword = $env:DRIVER_CERT_PASSWORD,
    # 'auto' (default) = required iff this is a v* tag build; 'true'/'false' to force. See below.
    [ValidateSet('auto', 'true', 'false')][string]$RequireSignedCert = 'auto',
    [switch]$SkipBuild,
    [ValidateSet('x64', 'arm64')][string]$Arch = 'x64'    # the drivers' target; the tools stay the runner's
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$PSNativeCommandUseErrorActionPreference = $false

# The decoded DRIVER_CERT key is trusted as a machine root on every box that installs punktfunk,
# so it must not outlive this script: the trap shreds it on any throw, then re-throws, and the
# normal exit calls Remove-SigningPfx too. signing.ps1 holds the shred; the key stays this script's.
. (Join-Path $PSScriptRoot 'signing.ps1')
trap { Remove-SigningPfx; break }

$DriversDir = (Resolve-Path $DriversDir).Path
$clear = Join-Path $PSScriptRoot 'clear-force-integrity.ps1'

$drivers = @(
    @{ crate = 'pf-gamepad';   dll = 'pf_gamepad.dll';   inx = 'pf-gamepad\pf_gamepad.inx';     inf = 'pf_gamepad.inf';   cat = 'pf_gamepad.cat' }
    @{ crate = 'pf-xusb';      dll = 'pf_xusb.dll';      inx = 'pf-xusb\pf_xusb.inx';           inf = 'pf_xusb.inf';      cat = 'pf_xusb.cat' }
    # Not a gamepad, but it rides the identical UMDF HID pipeline + the same install path
    # (`driver install --gamepad` adds every staged .inf): the resident virtual HID mouse that
    # keeps SM_MOUSEPRESENT true so DWM composites a cursor on headless hosts.
    @{ crate = 'pf-mouse';     dll = 'pf_mouse.dll';     inx = 'pf-mouse\pf_mouse.inx';         inf = 'pf_mouse.inf';     cat = 'pf_mouse.cat' }
)
foreach ($d in $drivers) {
    if (-not (Test-Path (Join-Path $DriversDir $d.inx))) { throw "no $($d.inx) under $DriversDir" }
}

# --- WDK build env ----------------------------------------------------------------------------
if (-not $env:Version_Number) { $env:Version_Number = '10.0.26100.0' }
if (-not $env:LIBCLANG_PATH -and (Test-Path 'C:\Program Files\LLVM\bin\libclang.dll')) {
    $env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin'
}
# Builds through drivers-cargo.ps1 like build-pf-vdisplay.ps1: the in-tree target dir from one fixed
# X: root, so a target kept between CI runs never sees two checkout paths.
$triple = if ($Arch -eq 'arm64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
$stampArch = if ($Arch -eq 'arm64') { 'arm64' } else { 'amd64' }
$catOs = if ($Arch -eq 'arm64') { '10_NI_ARM64' } else { '10_X64' }   # both floor at 22H2 (22621)
$rel = Join-Path $DriversDir "target\$triple\release"

# --- 1. build (release) - one build covers the whole workspace --------------------------------
if (-not $SkipBuild) {
    Write-Host "==> cargo build --release --target $triple (drivers workspace) in $DriversDir"
    & (Join-Path $PSScriptRoot 'drivers-cargo.ps1') "build --release --target $triple"
    $rc = $LASTEXITCODE
    if ($rc -ne 0) { throw "gamepad drivers cargo build failed ($rc)" }
}
foreach ($d in $drivers) {
    if (-not (Test-Path (Join-Path $rel $d.dll))) { throw "driver not built: $(Join-Path $rel $d.dll)" }
}

# --- 2. WDK sign tools ------------------------------------------------------------------------
$signtool = Find-SdkTool 'signtool.exe'
$stampinf = Find-SdkTool 'stampinf.exe'
$inf2cat = Find-SdkTool 'Inf2Cat.exe' 'x86'

# --- 3. signing cert (supplied stable pfx OR fresh self-signed; shared by both drivers) -------
# FAIL CLOSED on a real release, same rule as the host/MSIX pack scripts. The fallback below mints
# a cert per BUILD, and the installer trusts whatever .cer ships in the bundle - so the signature
# proves nothing about origin, and each upgrade adds another self-signed root CA to the user's
# machine under the same name. That is survivable for canary and dev builds; shipping it in a
# release is not. ('auto' resolves from GITHUB_REF so a new workflow inherits the guard.)
$requireCert = if ($RequireSignedCert -eq 'auto') { $env:GITHUB_REF -like 'refs/tags/v*' }
               else { [Convert]::ToBoolean($RequireSignedCert) }
$cleanupCert = $null
if ($CertPfxB64) {
    Write-Host '==> signing with supplied driver cert (DRIVER_CERT_PFX_B64)'
    $pfx = Join-Path (Split-Path -Parent $Out) 'driver-signing.pfx'
    $script:ShredPfx = $pfx
    [IO.File]::WriteAllBytes($pfx, [Convert]::FromBase64String($CertPfxB64))
    $sec = if ($CertPassword) { ConvertTo-SecureString $CertPassword -AsPlainText -Force } else { $null }
    $signArgs = @('/f', $pfx); if ($CertPassword) { $signArgs += @('/p', $CertPassword) }
    $pubForCer = if ($sec) { Get-PfxCertificate -FilePath $pfx -Password $sec } else { Get-PfxCertificate -FilePath $pfx }
}
elseif ($requireCert) {
    throw ("release build ($env:GITHUB_REF) with no DRIVER_CERT_PFX_B64 - refusing to sign drivers " +
           "with a per-build throwaway cert. Set the DRIVER_CERT_PFX_B64 / DRIVER_CERT_PASSWORD " +
           "secrets (packaging/windows/README.md), or pass -RequireSignedCert false for a test build.")
}
else {
    Write-Host '==> no DRIVER_CERT_PFX_B64 -> generating a fresh self-signed driver cert (the installer trusts the bundled .cer at install time)'
    $cleanupCert = New-SelfSignedCertificate -Type CodeSigningCert -Subject 'CN=punktfunk-driver' `
        -CertStoreLocation Cert:\CurrentUser\My -KeyExportPolicy Exportable -NotAfter (Get-Date).AddYears(10)
    $signArgs = @('/sha1', $cleanupCert.Thumbprint)
    $pubForCer = $cleanupCert
}

# --- 4. stage + clear FORCE_INTEGRITY + sign dlls + stampinf infs ------------------------------
if (Test-Path $Out) { Remove-Item $Out -Recurse -Force }
New-Item -ItemType Directory -Force -Path $Out | Out-Null
if (-not $DriverVer) { $now = Get-Date; $DriverVer = '9.9.{0}.{1}' -f $now.ToString('MMdd'), $now.ToString('HHmm') }

foreach ($d in $drivers) {
    $sDll = Join-Path $Out $d.dll
    $sInf = Join-Path $Out $d.inf
    Copy-Item (Join-Path $rel $d.dll) $sDll -Force
    Copy-Item (Join-Path $DriversDir $d.inx) $sInf -Force   # stampinf rewrites this copy in place
    # In-process, so the script's own throw propagates (a child powershell's exit is swallowed).
    & $clear -Path $sDll | Out-Null
    & $signtool sign /fd SHA256 @signArgs $sDll | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "signtool sign ($($d.dll)) failed ($LASTEXITCODE)" }
    & $stampinf -f $sInf -d '*' -a $stampArch -u '2.15.0' -v $DriverVer | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "stampinf ($($d.inf)) failed ($LASTEXITCODE)" }
}

# --- 5. Inf2Cat both catalogs (one pass over -Out), then sign each -----------------------------
& $inf2cat /driver:$Out /os:$catOs /uselocaltime | Out-Null
foreach ($d in $drivers) {
    $sCat = Join-Path $Out $d.cat
    if (-not (Test-Path $sCat)) { throw "Inf2Cat did not produce $sCat" }
    & $signtool sign /fd SHA256 @signArgs $sCat | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "signtool sign ($($d.cat)) failed ($LASTEXITCODE)" }
}

# --- 6. one shared public .cer ----------------------------------------------------------------
Export-Certificate -Cert $pubForCer -FilePath (Join-Path $Out 'punktfunk-driver.cer') | Out-Null
if ($cleanupCert) { Remove-Item "Cert:\CurrentUser\My\$($cleanupCert.Thumbprint)" -Force -ErrorAction SilentlyContinue }
Remove-SigningPfx

Write-Host "==> built + signed gamepad drivers  DriverVer=$DriverVer  ->  $Out"
Get-ChildItem $Out -File | ForEach-Object { "    $($_.Name)  ($($_.Length) bytes)" }
