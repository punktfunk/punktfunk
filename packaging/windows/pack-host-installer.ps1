<#
.SYNOPSIS
  Build + sign the punktfunk Windows host installer (the punktfunk-setup-win engine exe).

.DESCRIPTION
  From a release `cargo build -p punktfunk-host --features nvenc` output (the exe), this:
    1. resolves a signing backend - Azure Artifact Signing (formerly Trusted Signing) when the
       AZURE_CODESIGNING_* trio is set, else a supplied stable .pfx from CI secrets, else an
       ephemeral self-signed CN=unom - same scheme as the client's pack-msix.ps1. The .pfx paths
       also export the public .cer; Azure does not (see below). The ephemeral fallback is for
       canary/CI/dev ONLY: on a v* tag build a missing cert (or -NoSign) is a hard failure, never
       a silent downgrade to a throwaway cert - see -RequireSignedCert,
    2. stamps -Version into the inner punktfunk-host.exe (stamp-version.ps1), then signs it,
    3. stages the pf-vdisplay virtual-display driver bundle (unless -NoDriver),
    4. builds the wizard and packs it over the {app} tree as punktfunk-host-setup-<ver>.exe,
    5. signs the setup.exe (timestamped - MANDATORY under Azure signing, see Sign-File),
    6. emits HOST_SETUP_PATH / HOST_CER_PATH to GITHUB_ENV for the publish step. Azure signing
       emits no .cer: the chain is publicly trusted, so there is nothing for a user to import.
       Every consumer of HOST_CER_PATH already guards on Test-Path, so it is simply absent.

  NOTE the drivers are signed separately, by build-pf-vdisplay.ps1 / build-gamepad-drivers.ps1 with
  the DRIVER_CERT_* secret, and are NOT re-signed here (that would invalidate their catalogs). The
  installer's signature and the driver catalogs' signatures are independent by design - Windows
  verifies the first via SmartScreen/UAC and the second via PnP, and never requires a common signer.

  Idempotent; safe to re-run. Run on the Windows runner / dev box (MSVC + Windows SDK).

.EXAMPLE
  pwsh -File pack-host-installer.ps1 -Version 0.2.137 -TargetDir C:\t\release -OutDir C:\t\out
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Version,                 # e.g. 0.2.137 or 1.4.0 (free-form)
    [Parameter(Mandatory = $true)][string]$TargetDir,               # cargo --release dir (has punktfunk-host.exe)
    [string]$OutDir = (Join-Path $TargetDir 'installer'),
    # Subject for the EPHEMERAL self-signed fallback only. Azure signing carries its own subject
    # (the profile's verified CN/O), and nothing downstream of setup.exe compares the two - unlike
    # the MSIX, whose manifest Identity/@Publisher must match byte-for-byte. See pack-msix.ps1.
    [string]$Publisher = 'CN=unom',
    [string]$PfxBase64 = $env:MSIX_CERT_PFX_B64,                    # reuse the client's signing secret
    [string]$PfxPassword = $env:MSIX_CERT_PASSWORD,
    # Azure Artifact Signing (formerly Trusted Signing). All three must be set to select it; it then
    # takes precedence over any .pfx. Credentials come from the environment via DefaultAzureCredential
    # (AZURE_TENANT_ID / AZURE_CLIENT_ID / AZURE_CLIENT_SECRET) - never passed as arguments, so they
    # cannot leak into a process listing or a transcript.
    [string]$AzureEndpoint = $env:AZURE_CODESIGNING_ENDPOINT,       # e.g. https://neu.codesigning.azure.net/
    [string]$AzureAccount = $env:AZURE_CODESIGNING_ACCOUNT,         # signing account name
    [string]$AzureProfile = $env:AZURE_CODESIGNING_PROFILE,         # certificate profile name
    [string]$AzureDlib = $env:AZURE_CODESIGNING_DLIB,               # path to Azure.CodeSigning.Dlib.dll
    [string]$WebDir = $env:WEB_OUTPUT_DIR,                          # built web .output tree -> bundle the mgmt console
    [string]$ScriptingBundle = $env:SCRIPTING_BUNDLE,              # built runner-cli.js -> bundle the plugin/script runner
    [string]$BunExe = $env:BUN_EXE,                                # portable bun.exe runtime for the console + runner
    [switch]$NoDriver,                                              # build without the bundled pf-vdisplay driver
    [switch]$NoSign,                                                # skip signing (local debug)
    # 'auto' (default) = required iff this is a v* tag build; 'true'/'false' to force. See below.
    [ValidateSet('auto', 'true', 'false')][string]$RequireSignedCert = 'auto',
    # The installer's architecture (#298). -TargetDir must already hold that arch's host build;
    # everything built HERE (drivers, Vulkan layer, wizard) follows it. x64 keeps its file names.
    [ValidateSet('x64', 'arm64')][string]$Arch = 'x64'
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
# Keep the "check $LASTEXITCODE myself" model: a non-zero native exit must not throw before the
# script reads it.
$PSNativeCommandUseErrorActionPreference = $false
. (Join-Path $PSScriptRoot 'signing.ps1')
# A throw anywhere below must not leave the decoded signing key in $OutDir.
trap { Remove-SigningPfx; break }

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$triple = if ($Arch -eq 'arm64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
$archSuffix = if ($Arch -eq 'arm64') { '_arm64' } else { '' }
$exe = Join-Path $TargetDir 'punktfunk-host.exe'
if (-not (Test-Path $exe)) { throw "missing build artifact 'punktfunk-host.exe' in $TargetDir (did 'cargo build --release -p punktfunk-host --features nvenc' run?)" }
$trayExe = Join-Path $TargetDir 'punktfunk-tray.exe'
if (-not (Test-Path $trayExe)) { throw "missing build artifact 'punktfunk-tray.exe' in $TargetDir (did 'cargo build --release -p punktfunk-tray' run?)" }
# The seat keeper builds in its own workspace (crates/pf-seat-keeper) into the same target dir.
$keeperExe = Join-Path $TargetDir 'punktfunk-seat-keeper.exe'
if (-not (Test-Path $keeperExe)) { throw "missing build artifact 'punktfunk-seat-keeper.exe' in $TargetDir (did 'cargo build --release --manifest-path crates/pf-seat-keeper/Cargo.toml' run?)" }
# The host starts this beside itself to capture a monitor it did not create (a pinned monitor, a
# shared screen). Without it those sessions fail; everything else streams.
$workerExe = Join-Path $TargetDir 'punktfunk-capture-worker.exe'
if (-not (Test-Path $workerExe)) { throw "missing build artifact 'punktfunk-capture-worker.exe' in $TargetDir (did 'cargo build --release -p punktfunk-capture-worker' run?)" }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
# The version goes into a copy: cargo re-links its own output on the next build.
$stampedExe = Join-Path $OutDir 'punktfunk-host.exe'
Copy-Item -LiteralPath $exe -Destination $stampedExe -Force
& (Join-Path $here 'stamp-version.ps1') -Path $stampedExe -Version $Version
$exe = $stampedExe

# --- signing backend (signing.ps1: Azure, then MSIX_CERT_PFX_B64, then ephemeral) ----------------
# The .pfx modes export the .cer users import once (LocalMachine\TrustedPublisher) so SmartScreen
# and UAC trust the setup.exe. Azure emits none, so HOST_CER_PATH stays unset.
$cerPath = Join-Path $OutDir "punktfunk-host-windows_${Version}.cer"
$signing = Resolve-SigningMode -OutDir $OutDir -Publisher $Publisher -FriendlyName 'punktfunk host (self-signed)' `
    -PfxBase64 $PfxBase64 -PfxPassword $PfxPassword -AzureEndpoint $AzureEndpoint -AzureAccount $AzureAccount `
    -AzureProfile $AzureProfile -AzureDlib $AzureDlib -RequireSignedCert $RequireSignedCert `
    -CerPath $cerPath -NoSign:$NoSign

# --- sign the inner exes before they're packed -------------------------------------------------
Sign-File $signing $exe
Sign-File $signing $trayExe
Sign-File $signing $keeperExe
Sign-File $signing $workerExe

# --- resolve + validate the installer's source files ------------------------------------------
$repoRoot = (Resolve-Path (Join-Path $here '..\..')).Path
$hostEnvSrc = Join-Path $repoRoot 'scripts\windows\host.env.example'
$readmeSrc = Join-Path $here 'README.md'
$brandIco = Join-Path $here 'branding\punktfunk.ico'
foreach ($p in @($exe, $trayExe, $keeperExe, $workerExe, $hostEnvSrc, $readmeSrc, $brandIco)) {
    if (-not (Test-Path -LiteralPath $p)) { throw "installer source file missing: $p" }
}

# License/attribution payload bundled into {app}\licenses: the project's own MIT/Apache texts and the
# generated third-party crate notices. THIRD-PARTY-NOTICES.txt ships verbatim from
# the committed copy; nothing regenerates it here. ci.yml's THIRD-PARTY-NOTICES drift gate is
# what keeps that copy true.
$licStage = Join-Path $OutDir 'licenses'
New-Item -ItemType Directory -Force -Path $licStage | Out-Null
foreach ($n in @('LICENSE-MIT', 'LICENSE-APACHE', 'THIRD-PARTY-NOTICES.txt')) {
    $p = Join-Path $repoRoot $n
    if (Test-Path $p) { Copy-Item $p -Destination $licStage -Force }
    else { Write-Warning "license payload missing (skipped): $p" }
}

# --- build (from source) + stage the pf-vdisplay virtual-display driver -----------------------
# pf-vdisplay is our all-Rust IddCx driver (packaging/windows/drivers/). It is now BUILT FROM SOURCE
# every release (build-pf-vdisplay.ps1) instead of shipping a checked-in prebuilt binary: the vendored
# binary went stale (its .cat stopped covering an edited .inf -> pnputil SPAPI_E_FILE_HASH_NOT_IN_CATALOG
# on every box, and it predated IOCTL_SET_RENDER_ADAPTER the host needs on hybrid/Optimus GPUs). Building
# here keeps the .dll/.inf/.cat in lockstep + ships current driver features. stage-pf-vdisplay.ps1 then
# adds the fetched nefcon device tool. (Needs the WDK build env; -NoDriver skips it for a WDK-less pack.)
if (-not $NoDriver) {
    $built = Join-Path $OutDir 'pfvd-built'
    & (Join-Path $here 'build-pf-vdisplay.ps1') -Out $built -Arch $Arch
    $stage = Join-Path $OutDir 'stage'
    & (Join-Path $here 'stage-pf-vdisplay.ps1') -OutDir $stage -VendorDir $built -Arch $Arch
}
else { Write-Host "-NoDriver: building installer WITHOUT the bundled pf-vdisplay driver" }

# --- build (from source) + stage the punktfunk virtual-gamepad UMDF drivers --------------------
# pf-gamepad (DualSense / DS4 / Edge / Deck) + pf-xusb (Xbox 360 / XInput) are members of the same drivers
# workspace as pf-vdisplay, built from source per release (build-gamepad-drivers.ps1) - same anti-stale
# reasoning as pf-vdisplay; the prior checked-in binaries under gamepad-drivers/ are retired. The
# installer adds each to the store via `punktfunk-host.exe driver install --gamepad` (the host
# SwDeviceCreate's the per-session devnodes).
if (-not $NoDriver) {
    $gpBuilt = Join-Path $OutDir 'gamepad-built'
    # -SkipBuild: build-pf-vdisplay.ps1 above already `cargo build`s the WHOLE drivers workspace (incl.
    # the gamepad cdylibs), so just sign+stage them here - no redundant second full build.
    & (Join-Path $here 'build-gamepad-drivers.ps1') -Out $gpBuilt -SkipBuild -Arch $Arch
    $gpStage = Join-Path $OutDir 'gamepad'
    if (Test-Path $gpStage) { Remove-Item -Recurse -Force $gpStage }
    New-Item -ItemType Directory -Force -Path $gpStage | Out-Null
    Copy-Item (Join-Path $gpBuilt '*') $gpStage -Force
    Write-Host "==> built + staged gamepad UMDF drivers -> $gpStage"
}

# --- stage the official base VB-CABLE package (the streaming virtual microphone) --------------
# VB-CABLE is no longer bundled (the audio-substrate program, 2026-08): the host mints its own
# audio endpoints from Steam's streaming drivers ("Punktfunk Speakers/Microphone"), so audio needs
# Steam installed on the target box - never running - and no third-party cable. A user-installed
# VB-CABLE keeps working as a fallback mic target.

# --- stage the bun runtime + the two bun payloads (web console, plugin/script runner) --------------
# Both the web console and the runner run on bun. bun is staged ONCE into $OutDir and shared by the
# two payloads; each payload is omitted when its inputs are unset (e.g. a local debug pack), and the
# {app} tree below simply has no bun\ or web\ then.
$haveBun = $BunExe -and (Test-Path $BunExe)
$wantWeb = $WebDir -and (Test-Path $WebDir) -and $haveBun
$wantScripting = $ScriptingBundle -and (Test-Path $ScriptingBundle) -and $haveBun
if ($wantWeb -or $wantScripting) {
    $bunStage = Join-Path $OutDir 'bun.exe'
    Copy-Item -LiteralPath $BunExe -Destination $bunStage -Force
}
# The web console: the self-contained .output tree (Nitro noExternals - deps bundled + tree-shaken,
# no node_modules), run as a supervised child of the PunktfunkHost service (no launcher script),
# auto-wired to the host's loopback mgmt API.
if ($wantWeb) {
    $webStage = Join-Path $OutDir 'web'
    if (Test-Path $webStage) { Remove-Item $webStage -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $webStage | Out-Null
    Copy-Item (Join-Path $WebDir '*') -Destination $webStage -Recurse -Force
    Write-Host "bundling the web console from $WebDir (+ bun $BunExe)"
}
else { Write-Host "no -WebDir/-BunExe -> installer built WITHOUT the web console" }
# The plugin/script runner: one self-contained bundle (effect + the SDK inlined). Its scheduled task
# is registered DISABLED (opt-in) by the installer. Built by CI (SCRIPTING_BUNDLE) alongside the web
# console; omitted when -ScriptingBundle/-BunExe are unset.
if ($wantScripting) {
    $scrStage = Join-Path $OutDir 'scripting'
    if (Test-Path $scrStage) { Remove-Item $scrStage -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $scrStage | Out-Null
    $scrBundle = Join-Path $scrStage 'runner-cli.js'
    Copy-Item -LiteralPath $ScriptingBundle -Destination $scrBundle -Force
    $scrRun = Join-Path $scrStage 'scripting-run.cmd'
    Copy-Item (Join-Path $repoRoot 'scripts\windows\scripting-run.cmd') -Destination $scrRun -Force
    Write-Host "bundling the plugin/script runner from $ScriptingBundle (+ bun $BunExe)"
}
else { Write-Host "no -ScriptingBundle/-BunExe -> installer built WITHOUT the plugin/script runner" }

# --- build + stage the HDR Vulkan layer (pf-vkhdr-layer) --------------------------------------
# A tiny always-on Vulkan implicit layer (cdylib) that advertises HDR10/scRGB surface formats on the
# virtual display so Vulkan games (Doom: The Dark Ages, etc.) can enable HDR while streaming - the
# NVIDIA/AMD ICDs hide HDR formats on an indirect display even though they accept+present a forced HDR
# swapchain there. Self-gated on the display's actual advanced-color state, so it's a no-op on SDR.
# Standalone crate (own [workspace]); built here and registered by the installer. Skipped if cargo
# is unavailable or the build fails -> installer is produced WITHOUT the layer (non-fatal).
$layerSrc = Join-Path $here 'pf-vkhdr-layer'
if (Test-Path (Join-Path $layerSrc 'Cargo.toml')) {
    $layerTarget = Join-Path $OutDir 'vklayer-target'
    Write-Host "==> building pf-vkhdr-layer (cdylib)"
    $prevTarget = $env:CARGO_TARGET_DIR
    $env:CARGO_TARGET_DIR = $layerTarget
    Push-Location $layerSrc
    & cargo build --release --target $triple
    $layerExit = $LASTEXITCODE
    Pop-Location
    if ($prevTarget) { $env:CARGO_TARGET_DIR = $prevTarget } else { Remove-Item Env:\CARGO_TARGET_DIR -ErrorAction SilentlyContinue }
    $layerDll = Join-Path $layerTarget "$triple\release\pf_vkhdr_layer.dll"
    if ($layerExit -eq 0 -and (Test-Path $layerDll)) {
        $layerStage = Join-Path $OutDir 'vklayer'
        New-Item -ItemType Directory -Force -Path $layerStage | Out-Null
        Copy-Item $layerDll (Join-Path $layerStage 'pf_vkhdr_layer.dll') -Force
        Copy-Item (Join-Path $layerSrc 'pf_vkhdr_layer.json') (Join-Path $layerStage 'pf_vkhdr_layer.json') -Force
        Sign-File $signing (Join-Path $layerStage 'pf_vkhdr_layer.dll')
        Write-Host "==> staged pf-vkhdr-layer -> $layerStage"
    }
    else { Write-Warning "pf-vkhdr-layer build failed ($layerExit) - installer built WITHOUT the HDR Vulkan layer" }
}
else { Write-Host "no pf-vkhdr-layer crate -> installer built WITHOUT the HDR Vulkan layer" }

# --- build the wizard + pack the installer -----------------------------------------------------
$setup = Join-Path $OutDir "punktfunk-host-setup-$Version$archSuffix.exe"
# The wizard crate builds into a target dir of its own: windows-reactor-setup stages the
# self-contained WinAppSDK runtime next to the exe, and the packer takes everything in that
# dir that is not cargo's as the runtime set.
$wizTarget = Join-Path $OutDir 'wizard-target'
Write-Host "==> building punktfunk-setup-win (self-contained wizard + packer) -> $wizTarget"
$prevTarget = $env:CARGO_TARGET_DIR
$env:CARGO_TARGET_DIR = $wizTarget
Push-Location $repoRoot
# windows-reactor-setup extracts the WinAppSDK runtime into ONE cache under LOCALAPPDATA,
# keyed by package version only: whichever arch fills it first is what every later wizard
# build stages. A cross-built wizard therefore gets a cache of its own.
$prevLocal = $env:LOCALAPPDATA
if ($Arch -ne 'x64') { $env:LOCALAPPDATA = Join-Path $OutDir "reactor-cache-$Arch" }
& cargo build --release -p punktfunk-setup-win --target $triple
$wizExit = $LASTEXITCODE
$env:LOCALAPPDATA = $prevLocal
# The pack tool rewrites bytes on the runner, so a cross build needs a host-arch copy of it.
if ($wizExit -eq 0 -and $Arch -ne 'x64') {
    & cargo build --release -p punktfunk-setup-win --bin punktfunk-setup-pack
    $wizExit = $LASTEXITCODE
}
Pop-Location
if ($prevTarget) { $env:CARGO_TARGET_DIR = $prevTarget } else { Remove-Item Env:\CARGO_TARGET_DIR -ErrorAction SilentlyContinue }
if ($wizExit -ne 0) { throw "punktfunk-setup-win build failed ($wizExit)" }
$wizRel = Join-Path $wizTarget "$triple\release"
$wizExe = Join-Path $wizRel 'punktfunk-setup-win.exe'
$packer = if ($Arch -eq 'x64') { Join-Path $wizRel 'punktfunk-setup-pack.exe' } else { Join-Path $wizTarget 'release\punktfunk-setup-pack.exe' }

# The {app} tree. A missing input is simply absent - the plan's DeployFiles lays down whatever
# this assembles, verbatim, and the host's own probes decide what a partial tree can do.
$appStage = Join-Path $OutDir 'app'
if (Test-Path $appStage) { Remove-Item $appStage -Recurse -Force }
New-Item -ItemType Directory -Force -Path $appStage | Out-Null
Copy-Item $exe, $trayExe, $keeperExe, $workerExe, $hostEnvSrc -Destination $appStage -Force
Copy-Item $readmeSrc -Destination (Join-Path $appStage 'README.txt') -Force
Copy-Item $brandIco -Destination $appStage -Force
Copy-Item $licStage -Destination (Join-Path $appStage 'licenses') -Recurse -Force
if ($wantWeb -or $wantScripting) {
    New-Item -ItemType Directory -Force -Path (Join-Path $appStage 'bun') | Out-Null
    Copy-Item $bunStage -Destination (Join-Path $appStage 'bun\bun.exe') -Force
}
if ($wantWeb) { Copy-Item $webStage -Destination (Join-Path $appStage 'web\.output') -Recurse -Force }
if ($wantScripting) { Copy-Item $scrStage -Destination (Join-Path $appStage 'scripting') -Recurse -Force }
if ($layerStage -and (Test-Path $layerStage)) { Copy-Item $layerStage -Destination (Join-Path $appStage 'vklayer') -Recurse -Force }
# The seats display driver stays on disk beside the host, since the extracted staging tree below is
# deleted after setup. Setup never installs it: the console does, when the operator turns seats on.
if (-not $NoDriver) {
    $seatsDir = Join-Path $appStage 'staging\pfvdisplay'
    New-Item -ItemType Directory -Force -Path $seatsDir | Out-Null
    foreach ($f in 'pf_vdisplay_seats.inf', 'pf_vdisplay_seats.cat', 'pf_vdisplay.dll') { Copy-Item (Join-Path $stage $f) -Destination $seatsDir -Force }
}
# Driver payloads: extracted beside the wizard, handed to `driver install --dir <staging>\...`.
$stagingRoot = Join-Path $OutDir 'staging'
if (Test-Path $stagingRoot) { Remove-Item $stagingRoot -Recurse -Force }
New-Item -ItemType Directory -Force -Path $stagingRoot | Out-Null
if (-not $NoDriver) {
    Copy-Item $stage -Destination (Join-Path $stagingRoot 'pfvdisplay') -Recurse -Force
    Copy-Item $gpStage -Destination (Join-Path $stagingRoot 'gamepad') -Recurse -Force
}

# D6: the payload-less uninstaller lands in {app} as unins000.exe, signed before it is packed.
$unins = Join-Path $appStage 'unins000.exe'
& $packer pack-uninstaller --exe $wizExe --runtime $wizRel --version $Version --artifact host --out $unins
if ($LASTEXITCODE -ne 0) { throw "pack-uninstaller failed ($LASTEXITCODE)" }
Sign-File $signing $unins

& $packer pack --exe $wizExe --runtime $wizRel --app $appStage --staging $stagingRoot --version $Version --artifact host --out $setup
if ($LASTEXITCODE -ne 0) { throw "pack failed ($LASTEXITCODE)" }
& $packer inspect $setup
if ($LASTEXITCODE -ne 0) { throw "inspect failed ($LASTEXITCODE)" }
if (-not (Test-Path $setup)) { throw "expected installer not produced: $setup" }

# --- sign the setup.exe + clean up ------------------------------------------------------------
Sign-File $signing $setup
Remove-SigningPfx
Remove-Item $signing.Metadata -Force -ErrorAction SilentlyContinue

Write-Host ""
Write-Host "==> installer: $setup"
if ($signing.Mode -eq 'azure') {
    Write-Host "==> signed by a publicly trusted CA - nothing for users to import."
}
elseif ($signing.Mode -ne 'none') {
    Write-Host "==> trust the cert once per machine (self-signed builds), then the signed setup.exe is trusted:"
    Write-Host "    Import-Certificate -FilePath '$cerPath' -CertStoreLocation Cert:\LocalMachine\TrustedPublisher"
}
if ($env:GITHUB_ENV) {
    "HOST_SETUP_PATH=$setup" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
    "HOST_PACK_TOOL=$packer" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
    if ($signing.Mode -notin 'none', 'azure') { "HOST_CER_PATH=$cerPath" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8 }
}
