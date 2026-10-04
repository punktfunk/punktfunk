<#
.SYNOPSIS
  Build + sign the pf-vdisplay UMDF IddCx virtual-display driver FROM SOURCE, in CI, and stage it for the
  host installer. This REPLACES the old vendored-prebuilt-binary model (packaging/windows/pf-vdisplay/) -
  the binary went stale (frozen mid-June while the driver source kept moving), which silently shipped two
  field bugs: (1) the catalog no longer covered the edited INF (pnputil SPAPI_E_FILE_HASH_NOT_IN_CATALOG on
  every box), and (2) the binary predated IOCTL_SET_RENDER_ADAPTER that the host needs to pin the IDD render
  GPU on hybrid/Optimus boxes. Building every release from source keeps the .dll/.inf/.cat in lockstep and
  ships current driver features.

.DESCRIPTION
  Mirrors packaging/windows/drivers/deploy-dev.ps1 but for CI (release build, output to -Out, cert from a
  secret OR a fresh self-signed). Steps: cargo build (the wdk-sys/windows-drivers-rs driver workspace) ->
  CLEAR the FORCE_INTEGRITY PE bit (wdk-build links /INTEGRITYCHECK, which a non-EV cert can't satisfy) ->
  sign the .dll -> stampinf a strictly-increasing DriverVer into the INF -> Inf2Cat the catalog -> sign the
  catalog -> export the public .cer. Output (-Out): pf_vdisplay.{dll,inf,cat} + punktfunk-driver.cer,
  plus pf_vdisplay_seats.{inf,cat} - the same driver claiming the seat display ids, which only a
  box that runs seats installs (see below).

  Requires the WDK build env: cargo + the x64 MSVC toolset (its ARM64 cross compiler for -Arch arm64), an LLVM compatible with the driver's bindgen
  (>= 0.72 supports current clang), LIBCLANG_PATH, and the Windows 10/11 WDK (the runner has these). Sets
  Version_Number for wdk-build if the caller didn't.

.EXAMPLE
  pwsh -File build-pf-vdisplay.ps1 -Out C:\t\pfvd -DriverVer 9.9.0626.1612
#>
[CmdletBinding()]
param(
    [string]$DriversDir = (Join-Path $PSScriptRoot 'drivers'),
    [Parameter(Mandatory = $true)][string]$Out,
    [string]$DriverVer,                                   # default: 9.9.MMdd.HHmm (strictly-increasing)
    [string]$CertPfxB64 = $env:DRIVER_CERT_PFX_B64,       # optional stable driver-signing cert (CI secret)
    [string]$CertPassword = $env:DRIVER_CERT_PASSWORD,
    # 'auto' (default) = required iff this is a v* tag build; 'true'/'false' to force. See below.
    [ValidateSet('auto', 'true', 'false')][string]$RequireSignedCert = 'auto',
    [switch]$SkipBuild,                                   # reuse an existing target\...\release\pf_vdisplay.dll
    [ValidateSet('x64', 'arm64')][string]$Arch = 'x64'    # the driver's target; the tools stay the runner's
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$PSNativeCommandUseErrorActionPreference = $false

# The decoded signing key must not outlive this script. It is a STABLE key now - trusted as a
# machine root on every box that installs punktfunk - so a .pfx left behind in a build directory is
# a standing credential on a machine that runs build jobs, not the throwaway it used to be. A
# script-scope trap covers the failure paths; Remove-SigningPfx is also called on the way out.
$script:ShredPfx = $null
function Remove-SigningPfx {
    if ($script:ShredPfx -and (Test-Path $script:ShredPfx)) {
        Remove-Item $script:ShredPfx -Force -ErrorAction SilentlyContinue
        $script:ShredPfx = $null
    }
}
# `break` is for explicitness, not correctness: measured on the runner, a bare trap and
# trap+break behave identically here (exit 1, no resumption) for `throw` at script scope,
# `throw` inside a function, and a cmdlet error under EAP=Stop. Kept because it states the
# intent - shred, then re-throw - instead of relying on a default that is easy to misread.
trap { Remove-SigningPfx; break }

$DriversDir = (Resolve-Path $DriversDir).Path
$inx = Join-Path $DriversDir 'pf-vdisplay\pf_vdisplay.inx'
$clear = Join-Path $PSScriptRoot 'clear-force-integrity.ps1'
if (-not (Test-Path $inx)) { throw "no pf_vdisplay.inx under $DriversDir" }

# --- WDK build env (wdk-build needs Version_Number; bindgen needs LIBCLANG_PATH) --------------
if (-not $env:Version_Number) { $env:Version_Number = '10.0.26100.0' }
if (-not $env:LIBCLANG_PATH -and (Test-Path 'C:\Program Files\LLVM\bin\libclang.dll')) {
    $env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin'
}
# The build runs through drivers-cargo.ps1: it keeps the DEFAULT in-tree target dir (wdk-build
# walks up from OUT_DIR for a Cargo.lock and rejects a relocated CARGO_TARGET_DIR) while substing
# a drive onto the repo root, because the encoder deps build CMake projects whose MSBuild .tlog
# paths overflow MAX_PATH on a deep checkout. That script owns the reasoning; do not duplicate it.
$drvTarget = Join-Path $DriversDir 'target'
# ARM64 rides the same crt-static config (drivers/.cargo/config.toml) under an explicit --target.
$triple = if ($Arch -eq 'arm64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
$stampArch = if ($Arch -eq 'arm64') { 'arm64' } else { 'amd64' }
$catOs = if ($Arch -eq 'arm64') { '10_NI_ARM64' } else { '10_X64' }   # both floor at 22H2 (22621)
$dll = Join-Path $drvTarget "$triple\release\pf_vdisplay.dll"

# --- 1. build (release) -----------------------------------------------------------------------
if (-not $SkipBuild) {
    Write-Host "==> cargo build --release --target $triple (pf-vdisplay) in $DriversDir (-> $drvTarget)"
    & (Join-Path $PSScriptRoot 'drivers-cargo.ps1') "build --release --target $triple"
    $rc = $LASTEXITCODE
    if ($rc -ne 0) { throw "pf-vdisplay cargo build failed ($rc)" }
}
if (-not (Test-Path $dll)) { throw "driver not built: $dll" }

# --- 2. WDK sign tools ------------------------------------------------------------------------
$kits = 'C:\Program Files (x86)\Windows Kits\10\bin'
function Find-Tool([string]$name, [string]$arch) {
    (Get-ChildItem "$kits\*\$arch\$name" -ErrorAction SilentlyContinue | Sort-Object FullName | Select-Object -Last 1).FullName
}
$signtool = Find-Tool 'signtool.exe' 'x64'
$stampinf = Find-Tool 'stampinf.exe' 'x64'
$inf2cat = Find-Tool 'Inf2Cat.exe'  'x86'
foreach ($t in @($signtool, $stampinf, $inf2cat)) {
    if (-not $t) { throw 'a WDK tool (signtool/stampinf/Inf2Cat) was not found - install the Windows 10/11 WDK.' }
}

# --- 3. signing cert (supplied stable pfx OR fresh self-signed) -------------------------------
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
    $pfx = Join-Path $Out '..\driver-signing.pfx'
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

# --- 4. stage + clear FORCE_INTEGRITY + sign + cat --------------------------------------------
if (Test-Path $Out) { Remove-Item $Out -Recurse -Force }
New-Item -ItemType Directory -Force -Path $Out | Out-Null
$sDll = Join-Path $Out 'pf_vdisplay.dll'
$sInf = Join-Path $Out 'pf_vdisplay.inf'
$sCat = Join-Path $Out 'pf_vdisplay.cat'
$sCer = Join-Path $Out 'punktfunk-driver.cer'
Copy-Item $dll $sDll -Force
Copy-Item $inx $sInf -Force   # stampinf rewrites this copy in place

# The SEATS variant, built from the same .inx and signed with the same key. It exists because
# claiming `RdpIdd_IndirectDisplay` takes over EVERY RDP session on the machine, so it cannot ride
# the package everyone installs. Two packages, one build, and the operator chooses.
# It claims ONLY the seat ids, so it never competes for the console `Root\pf_vdisplay` node.
$sInfSeats = Join-Path $Out 'pf_vdisplay_seats.inf'
$sCatSeats = Join-Path $Out 'pf_vdisplay_seats.cat'
$seatsText = (Get-Content $inx -Raw).
    Replace('CatalogFile=pf_vdisplay.cat', 'CatalogFile=pf_vdisplay_seats.cat').
    Replace('%DeviceName%=pf_vdisplay_Install, Root\pf_vdisplay', '; console node: see pf_vdisplay.inf').
    Replace(';   %DeviceName%=pf_vdisplay_Seat_Install, RdpIdd_IndirectDisplay',
            '%DeviceName%=pf_vdisplay_Seat_Install, RdpIdd_IndirectDisplay')
if ($seatsText -notmatch '(?m)^%DeviceName%=pf_vdisplay_Seat_Install, RdpIdd_IndirectDisplay') {
    throw 'seats INF: the RdpIdd claim did not get uncommented - the .inx comment shape changed'
}
if ($seatsText -match '(?m)^%DeviceName%=pf_vdisplay_Install, Root\\pf_vdisplay') {
    throw 'seats INF: still claims the console node - it must claim only the seat ids'
}
Set-Content -Path $sInfSeats -Value $seatsText -NoNewline

# Clear FORCE_INTEGRITY BEFORE signing (it edits the PE, invalidating any signature). In-process,
# so the script's own throw propagates: a child powershell's exit code is swallowed here.
& $clear -Path $sDll | Out-Null

if (-not $DriverVer) { $now = Get-Date; $DriverVer = '9.9.{0}.{1}' -f $now.ToString('MMdd'), $now.ToString('HHmm') }

& $signtool sign /fd SHA256 @signArgs $sDll | Out-Null
if ($LASTEXITCODE -ne 0) { throw "signtool sign (dll) failed ($LASTEXITCODE)" }
& $stampinf -f $sInf -d '*' -a $stampArch -u '2.15.0' -v $DriverVer | Out-Null
if ($LASTEXITCODE -ne 0) { throw "stampinf (inf) failed ($LASTEXITCODE)" }
# Same DriverVer on both: the date is what wins `RdpIdd_IndirectDisplay` against the inbox driver,
# whose rank is identical to ours and whose date is frozen at 06/21/2006. A seats package stamped
# older than inbox loses the id silently (`windows-seat-display-tier.md` sec 5d).
& $stampinf -f $sInfSeats -d '*' -a $stampArch -u '2.15.0' -v $DriverVer | Out-Null
if ($LASTEXITCODE -ne 0) { throw "stampinf (seats inf) failed ($LASTEXITCODE)" }
& $inf2cat /driver:$Out /os:$catOs /uselocaltime | Out-Null
if (-not (Test-Path $sCat)) { throw "Inf2Cat did not produce $sCat" }
if (-not (Test-Path $sCatSeats)) { throw "Inf2Cat did not produce $sCatSeats" }
& $signtool sign /fd SHA256 @signArgs $sCat | Out-Null
if ($LASTEXITCODE -ne 0) { throw "signtool sign (cat) failed ($LASTEXITCODE)" }
& $signtool sign /fd SHA256 @signArgs $sCatSeats | Out-Null
if ($LASTEXITCODE -ne 0) { throw "signtool sign (seats cat) failed ($LASTEXITCODE)" }
Export-Certificate -Cert $pubForCer -FilePath $sCer | Out-Null
if ($cleanupCert) { Remove-Item "Cert:\CurrentUser\My\$($cleanupCert.Thumbprint)" -Force -ErrorAction SilentlyContinue }
Remove-SigningPfx

# --- 5. guard: assert the freshly-built catalog covers the inf + dll ---------------------------
# Built-from-source can't drift, but this catches a botched stampinf/Inf2Cat ordering. Test-FileCatalog
# itself can't always OPEN a catalog signed by a not-yet-trusted cert (it throws UnableToOpenCatalogFile),
# so treat ITS failure as inconclusive (warn) - but a real coverage miss still fails the build.
$cat = $null
try { $cat = Test-FileCatalog -CatalogFilePath $sCat -Path $Out -FilesToSkip 'pf_vdisplay.cat', 'pf_vdisplay_seats.cat', 'pf_vdisplay_seats.inf', 'punktfunk-driver.cer' -Detailed }
catch { Write-Warning "catalog coverage guard inconclusive (Test-FileCatalog: $($_.Exception.Message))" }
if ($cat) {
    $covered = @($cat.CatalogItems.Keys)
    foreach ($need in @('pf_vdisplay.inf', 'pf_vdisplay.dll')) {
        if (-not ($covered | Where-Object { $_ -like "*$need" })) {
            throw "catalog coverage guard: $need is NOT in $sCat (stampinf/Inf2Cat ordering bug?)"
        }
    }
    Write-Host "    catalog covers pf_vdisplay.inf + pf_vdisplay.dll (status=$($cat.Status))"
}

Write-Host "==> built + signed pf-vdisplay  DriverVer=$DriverVer  ->  $Out"
Get-ChildItem $Out -File | ForEach-Object { "    $($_.Name)  ($($_.Length) bytes)" }
