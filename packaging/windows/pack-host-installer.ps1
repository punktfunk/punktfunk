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
    2. signs the inner punktfunk-host.exe,
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
# Keep the traditional "check $LASTEXITCODE myself" model: don't let pwsh 7.4 turn a non-zero native
# exit into a terminating error (it would bypass Sign-File's timestamp-then-retry fallback below).
$PSNativeCommandUseErrorActionPreference = $false

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$triple = if ($Arch -eq 'arm64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
$archSuffix = if ($Arch -eq 'arm64') { '_arm64' } else { '' }
$exe = Join-Path $TargetDir 'punktfunk-host.exe'
if (-not (Test-Path $exe)) { throw "missing build artifact 'punktfunk-host.exe' in $TargetDir (did 'cargo build --release -p punktfunk-host --features nvenc' run?)" }
$trayExe = Join-Path $TargetDir 'punktfunk-tray.exe'
if (-not (Test-Path $trayExe)) { throw "missing build artifact 'punktfunk-tray.exe' in $TargetDir (did 'cargo build --release -p punktfunk-tray' run?)" }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

# --- locate signtool (Windows SDK) -----------------------------------------------------------
function Find-SdkTool([string]$name) {
    $root = 'C:\Program Files (x86)\Windows Kits\10\bin'
    $hit = Get-ChildItem -Path $root -Recurse -Filter $name -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match '\\(10\.0\.\d+\.\d+)\\x64\\' } |
        Sort-Object { [version]([regex]::Match($_.FullName, '\\(10\.0\.\d+\.\d+)\\x64\\').Groups[1].Value) } |
        Select-Object -Last 1
    if (-not $hit) { throw "$name not found under $root - install the Windows 10/11 SDK." }
    $hit.FullName
}
# Azure.CodeSigning.Dlib.dll ships in the Microsoft.Trusted.Signing.Client NuGet package, which has no
# installer and no fixed location - hence an explicit override first, then the two paths the runner
# setup uses (see packaging/windows/README.md). Newest version wins so a package update is picked up
# without editing this script.
function Find-AzureDlib([string]$Explicit) {
    if ($Explicit) {
        if (-not (Test-Path $Explicit)) { throw "AZURE_CODESIGNING_DLIB points at a missing file: $Explicit" }
        return (Resolve-Path $Explicit).Path
    }
    $roots = @(
        (Join-Path $env:USERPROFILE '.nuget\packages\microsoft.trusted.signing.client'),
        'C:\trusted-signing\microsoft.trusted.signing.client'
    ) | Where-Object { $_ -and (Test-Path $_) }
    $hit = $roots | ForEach-Object { Get-ChildItem -Path $_ -Recurse -Filter 'Azure.CodeSigning.Dlib.dll' -ErrorAction SilentlyContinue } |
        Where-Object { $_.FullName -match '\\bin\\x64\\' } |
        Sort-Object LastWriteTime | Select-Object -Last 1
    if (-not $hit) {
        throw ("Azure.CodeSigning.Dlib.dll not found. Install the signing client on this box, e.g. " +
               "``nuget install Microsoft.Trusted.Signing.Client -OutputDirectory " +
               "`$env:USERPROFILE\.nuget\packages``, or set AZURE_CODESIGNING_DLIB to its full path.")
    }
    $hit.FullName
}

# --- signing cert (supplied stable pfx OR ephemeral self-signed) -----------------------------
# FAIL CLOSED on a real release. The ephemeral fallback below exists so canary/CI/dev builds keep
# working without the secret, but it is a per-build throwaway cert: nobody can pin it, and an
# installer signed with one is indistinguishable from one signed by an attacker. Silently falling
# back on a tag build would ship exactly that to users under the release's name - so on refs/tags/v*
# a missing MSIX_CERT_PFX_B64 (or -NoSign) is a build failure, not a downgrade. ('auto' resolves from
# GITHUB_REF so a workflow can't forget to opt in; -RequireSignedCert true/false overrides.)
$requireCert = if ($RequireSignedCert -eq 'auto') { $env:GITHUB_REF -like 'refs/tags/v*' }
               else { [Convert]::ToBoolean($RequireSignedCert) }
if ($NoSign -and $requireCert) {
    throw "release build ($env:GITHUB_REF) with -NoSign - refusing to publish an unsigned installer."
}
$pfxPath = Join-Path $OutDir 'signing.pfx'
$cerPath = Join-Path $OutDir "punktfunk-host-windows_${Version}.cer"
$azureMetadata = Join-Path $OutDir 'azure-codesigning.json'
$signMode = 'none'
$signtool = $null
if (-not $NoSign) {
    $signtool = Find-SdkTool 'signtool.exe'
    Write-Host "signtool: $signtool"
    if ($AzureEndpoint -and $AzureAccount -and $AzureProfile) {
        $signMode = 'azure'
        $AzureDlib = Find-AzureDlib $AzureDlib
        # signtool reads the account/profile from this file (/dmdf) rather than the command line.
        @{
            Endpoint                = $AzureEndpoint
            CodeSigningAccountName  = $AzureAccount
            CertificateProfileName  = $AzureProfile
        } | ConvertTo-Json | Set-Content -Path $azureMetadata -Encoding utf8
        Write-Host "signing via Azure Artifact Signing: $AzureAccount/$AzureProfile at $AzureEndpoint"
        Write-Host "  dlib: $AzureDlib"
        foreach ($v in 'AZURE_TENANT_ID', 'AZURE_CLIENT_ID', 'AZURE_CLIENT_SECRET') {
            if (-not [Environment]::GetEnvironmentVariable($v)) {
                throw ("Azure signing selected but $v is not set. The dlib authenticates with " +
                       "DefaultAzureCredential; without the service-principal trio it falls through to " +
                       "an interactive login that cannot complete on a runner and hangs the build.")
            }
        }
    }
    elseif ($PfxBase64) {
        $signMode = 'pfx'
        Write-Host "signing with supplied code-signing cert (MSIX_CERT_PFX_B64)"
        [IO.File]::WriteAllBytes($pfxPath, [Convert]::FromBase64String($PfxBase64))
    }
    elseif ($requireCert) {
        throw ("release build ($env:GITHUB_REF) with neither AZURE_CODESIGNING_* nor MSIX_CERT_PFX_B64 - " +
               "refusing to fall back to an ephemeral self-signed cert. Restore the signing secrets " +
               "(packaging/windows/README.md), or pass -RequireSignedCert false if this really is a test build.")
    }
    else {
        $signMode = 'selfsigned'
        Write-Host "no MSIX_CERT_PFX_B64 -> generating an ephemeral self-signed cert (subject $Publisher)"
        if (-not $PfxPassword) { $PfxPassword = 'punktfunk' }
        $tmp = New-SelfSignedCertificate -Type Custom -Subject $Publisher `
            -KeyUsage DigitalSignature -FriendlyName 'punktfunk host (self-signed)' `
            -CertStoreLocation 'Cert:\CurrentUser\My' `
            -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.3', '2.5.29.19={text}')
        $sec = ConvertTo-SecureString -String $PfxPassword -Force -AsPlainText
        Export-PfxCertificate -Cert "Cert:\CurrentUser\My\$($tmp.Thumbprint)" -FilePath $pfxPath -Password $sec | Out-Null
        Remove-Item "Cert:\CurrentUser\My\$($tmp.Thumbprint)" -Force
    }
    # Export the public .cer for the .pfx-backed modes. For a self-signed cert it's the file users
    # import once (LocalMachine\TrustedPublisher) so SmartScreen/UAC trusts the signed setup.exe.
    # Azure signing has no .pfx to read and needs no import - the chain is publicly trusted - so it
    # deliberately produces no .cer and HOST_CER_PATH stays unset.
    if ($signMode -ne 'azure') {
        $pwsec = if ($PfxPassword) { ConvertTo-SecureString -String $PfxPassword -Force -AsPlainText } else { $null }
        $pubCert = if ($pwsec) { Get-PfxCertificate -FilePath $pfxPath -Password $pwsec } else { Get-PfxCertificate -FilePath $pfxPath }
        Export-Certificate -Cert $pubCert -FilePath $cerPath | Out-Null
        Write-Host "signing cert subject=$($pubCert.Subject) thumbprint=$($pubCert.Thumbprint)"
    }
}

# A timestamp is best-effort for a .pfx whose cert outlives the release, but MANDATORY under Azure
# signing: those leaf certs are minted per request and expire in ~3 days, so an untimestamped
# signature stops verifying within days of shipping. Retrying without one there would produce an
# artifact that passes on the runner and fails on every user's machine that weekend - so the
# fallback is gated on the mode rather than applied blindly.
function Sign-File([string]$Path) {
    if ($NoSign) { return }
    if ($signMode -eq 'azure') {
        $signArgs = @('sign', '/fd', 'SHA256', '/dlib', $AzureDlib, '/dmdf', $azureMetadata)
        $ts = 'http://timestamp.acs.microsoft.com'
    }
    else {
        $signArgs = @('sign', '/fd', 'SHA256', '/f', $pfxPath)
        if ($PfxPassword) { $signArgs += @('/p', $PfxPassword) }
        $ts = 'http://timestamp.digicert.com'
    }
    & $signtool ($signArgs + @('/tr', $ts, '/td', 'SHA256', $Path))
    if ($LASTEXITCODE -eq 0) { return }
    if ($signMode -eq 'azure') {
        throw ("timestamped sign failed for $Path ($LASTEXITCODE) - NOT retrying without a timestamp. " +
               "An Azure signing cert is valid for ~3 days; an untimestamped signature would go " +
               "untrusted within days of release.")
    }
    Write-Warning "timestamped sign failed for $Path - retrying without a timestamp"
    & $signtool ($signArgs + @($Path))
    if ($LASTEXITCODE -ne 0) { throw "signtool sign failed for $Path ($LASTEXITCODE)" }
}

# --- sign the inner exes before they're packed -------------------------------------------------
Sign-File $exe
Sign-File $trayExe

# --- resolve + validate the installer's source files ------------------------------------------
$repoRoot = (Resolve-Path (Join-Path $here '..\..')).Path
$hostEnvSrc = Join-Path $repoRoot 'scripts\windows\host.env.example'
$readmeSrc = Join-Path $here 'README.md'
$brandIco = Join-Path $here 'branding\punktfunk.ico'
foreach ($p in @($exe, $trayExe, $hostEnvSrc, $readmeSrc, $brandIco)) {
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
        Sign-File (Join-Path $layerStage 'pf_vkhdr_layer.dll')
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
Copy-Item $exe, $trayExe, $hostEnvSrc -Destination $appStage -Force
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
Sign-File $unins

& $packer pack --exe $wizExe --runtime $wizRel --app $appStage --staging $stagingRoot --version $Version --artifact host --out $setup
if ($LASTEXITCODE -ne 0) { throw "pack failed ($LASTEXITCODE)" }
& $packer inspect $setup
if ($LASTEXITCODE -ne 0) { throw "inspect failed ($LASTEXITCODE)" }
if (-not (Test-Path $setup)) { throw "expected installer not produced: $setup" }

# --- sign the setup.exe + clean up ------------------------------------------------------------
Sign-File $setup
Remove-Item $pfxPath -Force -ErrorAction SilentlyContinue
Remove-Item $azureMetadata -Force -ErrorAction SilentlyContinue

Write-Host ""
Write-Host "==> installer: $setup"
if ($signMode -eq 'azure') {
    Write-Host "==> signed by a publicly trusted CA - nothing for users to import."
}
elseif (-not $NoSign) {
    Write-Host "==> trust the cert once per machine (self-signed builds), then the signed setup.exe is trusted:"
    Write-Host "    Import-Certificate -FilePath '$cerPath' -CertStoreLocation Cert:\LocalMachine\TrustedPublisher"
}
if ($env:GITHUB_ENV) {
    "HOST_SETUP_PATH=$setup" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
    "HOST_PACK_TOOL=$packer" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
    if (-not $NoSign -and $signMode -ne 'azure') { "HOST_CER_PATH=$cerPath" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8 }
}
