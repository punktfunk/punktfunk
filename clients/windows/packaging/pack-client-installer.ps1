<#
.SYNOPSIS
  Pack + sign the punktfunk Windows client as a punktfunk-setup-client exe (the default download)
  and a portable .zip, from the layout pack-msix.ps1 already assembled.

.DESCRIPTION
  Runs AFTER pack-msix.ps1 in the same job and consumes its $OutDir\layout verbatim — one assembly,
  three artifacts (.msix, setup.exe, portable .zip). Why the installer exists at all: the MSIX
  install shape (WindowsApps ACLs + alias-only activation) breaks Steam's non-Steam-game picker,
  the Steam overlay's injection, and Big Picture launching; the installer's stable
  %LOCALAPPDATA%\Programs\Punktfunk path is what fixes all three.

  Steps:
    1. stage the runtime file set from -LayoutDir (drops AppxManifest.xml + the tile Assets),
    2. sign the four exes and SDL3.dll individually (the MSIX only signs its container),
    3. zip the stage -> the portable build,
    4. pack the unelevated client wizard over the same stage, sign the setup.exe,
    5. emit CLIENT_SETUP_PATH / CLIENT_ZIP_PATH to GITHUB_ENV for the publish step.

  Signing goes through packaging/windows/signing.ps1, shared with pack-msix.ps1 and
  pack-host-installer.ps1 (Azure Artifact Signing -> supplied .pfx -> ephemeral self-signed; fail
  closed on v* tags). No .cer is exported here: unlike an MSIX, a plain exe RUNS regardless of
  signer trust — an untrusted signature only costs a SmartScreen warning, so canary self-signed
  builds need nothing imported.

.EXAMPLE
  pwsh -File pack-client-installer.ps1 -Version 0.2.137.0 -Arch x64 `
    -LayoutDir C:\t\msix\layout -OutDir C:\t\installer
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Version,                 # 4-part numeric, same as the MSIX
    [Parameter(Mandatory = $true)][string]$LayoutDir,               # pack-msix.ps1's $OutDir\layout
    [ValidateSet('x64', 'arm64')][string]$Arch = 'x64',
    [string]$OutDir = (Join-Path (Split-Path -Parent $LayoutDir) 'installer'),
    # Subject for the EPHEMERAL self-signed fallback only; Azure/pfx carry their own subjects.
    [string]$Publisher = "CN=unom - Enrico B$([char]0xFC)hler, O=unom - Enrico B$([char]0xFC)hler, L=Rottweil, S=Baden-W$([char]0xFC)rttemberg, C=DE",
    [string]$PfxBase64 = $env:MSIX_CERT_PFX_B64,                    # reuse the client's signing secret
    [string]$PfxPassword = $env:MSIX_CERT_PASSWORD,
    [string]$AzureEndpoint = $env:AZURE_CODESIGNING_ENDPOINT,
    [string]$AzureAccount = $env:AZURE_CODESIGNING_ACCOUNT,
    [string]$AzureProfile = $env:AZURE_CODESIGNING_PROFILE,
    [string]$AzureDlib = $env:AZURE_CODESIGNING_DLIB,
    [ValidateSet('auto', 'true', 'false')][string]$RequireSignedCert = 'auto',
    [switch]$NoSign                                                 # skip signing (local debug)
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
# Keep the "check $LASTEXITCODE myself" model: a non-zero native exit must not throw before the
# script reads it.
$PSNativeCommandUseErrorActionPreference = $false
. (Join-Path $PSScriptRoot '..\..\..\packaging\windows\signing.ps1')
# A throw anywhere below must not leave the decoded signing key in $OutDir.
trap { Remove-SigningPfx; break }

if ($Version -notmatch '^\d+\.\d+\.\d+\.\d+$') {
    throw "Version must be 4-part numeric (Major.Minor.Build.Revision); got '$Version'."
}

$here = Split-Path -Parent $MyInvocation.MyCommand.Path

# --- stage the runtime file set (the portable layout = what the installer lays down) ----------
# Explicit list, not a wildcard copy: the MSIX layout also holds AppxManifest.xml and the tile
# Assets, which mean nothing outside a package (the exes embed their icons via build.rs).
# The ONE Assets\ file that does matter unpackaged is the Lucide icon font: the shell loads it
# via ms-appx:///Assets/lucide.ttf (app/lucide.rs), and unpackaged that URI resolves to the exe
# directory — without Assets\lucide.ttf every icon in the shell renders as a private-use box.
$required = @('punktfunk-client.exe', 'punktfunk-session.exe', 'punktfunk-console.exe', 'punktfunk.exe',
              'Microsoft.WindowsAppRuntime.Bootstrap.dll', 'SDL3.dll', 'resources.pri',
              'Assets\lucide.ttf')
$stage = Join-Path $OutDir 'portable'
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path (Join-Path $stage 'Assets') | Out-Null
foreach ($f in $required) {
    $src = Join-Path $LayoutDir $f
    if (-not (Test-Path $src)) { throw "missing '$f' in $LayoutDir (did pack-msix.ps1 run first?)" }
    Copy-Item $src (Join-Path $stage $f) -Force
}
$licSrc = Join-Path $LayoutDir 'licenses'
if (-not (Test-Path $licSrc)) { throw "missing licenses\ in $LayoutDir (did pack-msix.ps1 run first?)" }
Copy-Item $licSrc (Join-Path $stage 'licenses') -Recurse -Force

# --- signing backend (signing.ps1: Azure, then MSIX_CERT_PFX_B64, then ephemeral) -------------
$signing = Resolve-SigningMode -OutDir $OutDir -Publisher $Publisher `
    -FriendlyName 'punktfunk client installer (self-signed)' `
    -PfxBase64 $PfxBase64 -PfxPassword $PfxPassword -AzureEndpoint $AzureEndpoint -AzureAccount $AzureAccount `
    -AzureProfile $AzureProfile -AzureDlib $AzureDlib -RequireSignedCert $RequireSignedCert -NoSign:$NoSign

# --- sign the inner exes, zip the stage (portable build), then build + sign the installer ------
# SDL3.dll arrives unsigned; the WinAppRuntime bootstrap is already Microsoft-signed.
foreach ($f in $required | Where-Object { $_ -like '*.exe' -or $_ -eq 'SDL3.dll' }) {
    Sign-File $signing (Join-Path $stage $f)
}

$zip = Join-Path $OutDir "punktfunk-client-windows_${Version}_${Arch}-portable.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip
Write-Host "==> portable zip: $zip"

$setup = Join-Path $OutDir "punktfunk-client-setup-${Version}_${Arch}.exe"
# The wizard crate builds for this arch into a target dir of its own; the packer takes the
# self-contained runtime staged there and the CLIENT twin (asInvoker - never elevates).
$triple = if ($Arch -eq 'arm64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
$repoRoot = (Resolve-Path (Join-Path $here '..\..\..')).Path
$wizTarget = Join-Path $OutDir 'wizard-target'
Write-Host "==> building punktfunk-setup-client ($triple) -> $wizTarget"
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
if ($wizExit -eq 0 -and $triple -ne 'x86_64-pc-windows-msvc') {
    # The packer itself runs on the (x64) runner, whatever arch it packs for.
    & cargo build --release -p punktfunk-setup-win --bin punktfunk-setup-pack --target x86_64-pc-windows-msvc
    $wizExit = $LASTEXITCODE
}
Pop-Location
if ($prevTarget) { $env:CARGO_TARGET_DIR = $prevTarget } else { Remove-Item Env:\CARGO_TARGET_DIR -ErrorAction SilentlyContinue }
if ($wizExit -ne 0) { throw "punktfunk-setup-win build failed ($wizExit)" }
$wizRel = Join-Path $wizTarget "$triple\release"
$wizExe = Join-Path $wizRel 'punktfunk-setup-client.exe'
$packer = Join-Path $wizTarget 'x86_64-pc-windows-msvc\release\punktfunk-setup-pack.exe'
# D6: the payload-less uninstaller lands in {app} (the stage IS the {app} tree; the portable
# zip above was cut before it arrived), signed before it is packed.
$unins = Join-Path $stage 'unins000.exe'
& $packer pack-uninstaller --exe $wizExe --runtime $wizRel --version $Version --artifact client --out $unins
if ($LASTEXITCODE -ne 0) { throw "pack-uninstaller failed ($LASTEXITCODE)" }
Sign-File $signing $unins
& $packer pack --exe $wizExe --runtime $wizRel --app $stage --version $Version --artifact client --out $setup
if ($LASTEXITCODE -ne 0) { throw "pack failed ($LASTEXITCODE)" }
& $packer inspect $setup
if ($LASTEXITCODE -ne 0) { throw "inspect failed ($LASTEXITCODE)" }
if (-not (Test-Path $setup)) { throw "expected installer not produced: $setup" }
Sign-File $signing $setup
Remove-SigningPfx
Remove-Item $signing.Metadata -Force -ErrorAction SilentlyContinue

Write-Host ""
Write-Host "==> installer: $setup"
if ($signing.Mode -eq 'azure') {
    Write-Host "==> signed by a publicly trusted CA."
}
elseif ($signing.Mode -ne 'none') {
    Write-Host "==> $($signing.Mode)-signed: the exe still runs everywhere; expect a SmartScreen prompt on canary builds."
}
if ($env:GITHUB_ENV) {
    "CLIENT_SETUP_PATH=$setup" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
    "CLIENT_ZIP_PATH=$zip" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
}
