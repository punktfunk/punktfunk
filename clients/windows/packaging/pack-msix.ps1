<#
.SYNOPSIS
  Assemble, pack and sign the punktfunk Windows client as a signed MSIX.

.DESCRIPTION
  Builds a packaging layout from a release `cargo build` output (exe + the reactor/SDL3 auto-staged
  DLLs + resources.pri + the checked-in Assets + the manifest), runs makeappx, and
  signs with signtool. Idempotent; safe to re-run.

  NO FFmpeg DLLs since M10 (design/client-native-decode.md §6): the client decodes natively
  (pf-vkdecode / pf-dxvadec / openh264+rav1d) and link-imports no libav* at all, so the
  wildcard copy and its LGPL notice are gone with it. The HOST installer is unchanged —
  packaging/windows/pack-host-installer.ps1 still ships them for its amf-qsv encode path.

  Signing cert precedence:
    0. Azure Artifact Signing (formerly Trusted Signing) when AZURE_CODESIGNING_ENDPOINT/_ACCOUNT/
       _PROFILE are all set. HSM-backed, so there is no .pfx and nothing to export: the chain is
       publicly trusted, so no .cer is produced and MSIX_CER_PATH stays unset.
    1. -PfxBase64 / -PfxPassword  (a real or shared code-signing cert, e.g. from CI secrets) — the
       cert's subject DN MUST match -Publisher (which is stamped into the manifest Identity).
    2. otherwise an EPHEMERAL self-signed code-signing cert with subject = -Publisher is generated
       in-process. The package installs only where that cert is trusted, so the matching public
       .cer is exported next to the .msix for the user to import (Trusted People) before install.
       This fallback is for canary/CI/dev ONLY: on a v* tag build a missing cert is a hard failure
       (-RequireSignedCert), never a silent downgrade to a throwaway cert.

  WHICHEVER mode runs, the signed .msix is read back and its signer subject compared to -Publisher;
  a mismatch fails the build. MSIX package identity is Name + Publisher, so a publisher that does
  not match the signer is not a cosmetic problem — Add-AppxPackage rejects the package outright,
  and it would only be discovered by a user trying to install the release.

  Run on the Windows runner (or the dev VM) with the MSVC/Windows SDK present.

.EXAMPLE
  # x64 (default arch):
  pwsh -File pack-msix.ps1 -Version 0.2.137.0 -TargetDir C:\t\x86_64-pc-windows-msvc\release -OutDir C:\t\msix
  # arm64 (point -TargetDir at the ARM64 build):
  pwsh -File pack-msix.ps1 -Version 0.2.137.0 -Arch arm64 -TargetDir C:\t-a64\aarch64-pc-windows-msvc\release -OutDir C:\t-a64\msix
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Version,                     # 4-part numeric, e.g. 0.2.137.0
    [Parameter(Mandatory = $true)][string]$TargetDir,                   # cargo --release output dir (has the exe)
    [ValidateSet('x64', 'arm64')][string]$Arch = 'x64',                 # package ProcessorArchitecture + artifact suffix
    [string]$OutDir = (Join-Path $TargetDir 'msix'),
    # MUST equal the signing cert subject DN — this is the verified subject the Azure 'unom-io'
    # certificate profile issues. The 'ü' is written as an escape, not a literal: this file is UTF-8
    # with no BOM, and read by anything other than pwsh 7 a literal would silently mojibake into a
    # publisher that no longer matches the signer, which surfaces only as an Add-AppxPackage refusal
    # on a user's machine. Verified against the real signer after signing below.
    [string]$Publisher = "CN=unom - Enrico B$([char]0xFC)hler, O=unom - Enrico B$([char]0xFC)hler, L=Rottweil, S=Baden-W$([char]0xFC)rttemberg, C=DE",
    [string]$PfxBase64 = $env:MSIX_CERT_PFX_B64,                        # optional: base64 of a code-signing .pfx
    [string]$PfxPassword = $env:MSIX_CERT_PASSWORD,
    # Azure Artifact Signing. All three select it, ahead of any .pfx. Credentials arrive through the
    # environment via DefaultAzureCredential (AZURE_TENANT_ID / AZURE_CLIENT_ID / AZURE_CLIENT_SECRET)
    # rather than as arguments, so they cannot leak into a process listing or a transcript.
    [string]$AzureEndpoint = $env:AZURE_CODESIGNING_ENDPOINT,           # e.g. https://neu.codesigning.azure.net/
    [string]$AzureAccount = $env:AZURE_CODESIGNING_ACCOUNT,             # signing account name
    [string]$AzureProfile = $env:AZURE_CODESIGNING_PROFILE,             # certificate profile name
    [string]$AzureDlib = $env:AZURE_CODESIGNING_DLIB,                   # path to Azure.CodeSigning.Dlib.dll
    # 'auto' (default) = required iff this is a v* tag build; 'true'/'false' to force. See below.
    [ValidateSet('auto', 'true', 'false')][string]$RequireSignedCert = 'auto'
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
. (Join-Path $PSScriptRoot '..\..\..\packaging\windows\signing.ps1')
# A throw anywhere below must not leave the decoded signing key in $OutDir.
trap { Remove-SigningPfx; break }

if ($Version -notmatch '^\d+\.\d+\.\d+\.\d+$') {
    throw "Version must be 4-part numeric (Major.Minor.Build.Revision); got '$Version'."
}

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$assets = Join-Path $here 'assets'
$manifestTemplate = Join-Path $here 'AppxManifest.xml'

# The toolset's redistributable CRT for one arch (...\VC\Redist\MSVC\<ver>\<arch>\Microsoft.VC143.CRT).
# VCToolsRedistDir is what vcvars exports; without it, ask vswhere for every VS/Build Tools install
# and take the newest redist found. The DLLs must be at least as new as the STL that built Skia.
function Find-VcRedistCrt([string]$arch) {
    $roots = @()
    if ($env:VCToolsRedistDir) { $roots += $env:VCToolsRedistDir }
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path $vswhere) {
        $roots += & $vswhere -products * -prerelease -property installationPath |
            ForEach-Object { Join-Path $_ 'VC\Redist\MSVC' }
    }
    $hit = $roots | Where-Object { Test-Path $_ } |
        ForEach-Object { Get-ChildItem -Path $_ -Recurse -Directory -Filter 'Microsoft.VC143.CRT' -ErrorAction SilentlyContinue } |
        Where-Object { $_.FullName -match "\\$arch\\Microsoft\.VC143\.CRT$" } |
        # Newest version wins; at equal version the desktop set beats onecore\<arch> (Build Tools
        # ships arm64 only under onecore, so that one stays as the fallback).
        Sort-Object { [version]([regex]::Match($_.FullName, '\\(\d+\.\d+\.\d+)\\').Groups[1].Value) }, { $_.FullName -notmatch '\\onecore\\' } |
        Select-Object -Last 1
    if (-not $hit) { throw "Microsoft.VC143.CRT for $arch not found under $($roots -join '; ') - install the MSVC v143 toolset." }
    $hit.FullName
}
$makeappx = Find-SdkTool 'makeappx.exe'
Write-Host "makeappx: $makeappx"

# --- assemble the package layout ---
$layout = Join-Path $OutDir 'layout'
if (Test-Path $layout) { Remove-Item -Recurse -Force $layout }
New-Item -ItemType Directory -Force -Path (Join-Path $layout 'Assets') | Out-Null

# binaries + auto-staged runtime bits (reactor stages the App SDK bootstrap DLL + resources.pri,
# the sdl3 crate stages SDL3.dll — see crate build output). punktfunk-session.exe is the Vulkan
# session client the shell spawns for every stream (sibling resolution — see clients/windows/
# src/spawn.rs); Skia links statically and vulkan-1.dll is a GPU-driver component.
$required = @('punktfunk-client.exe', 'punktfunk-session.exe', 'punktfunk-console.exe', 'punktfunk.exe', 'Microsoft.WindowsAppRuntime.Bootstrap.dll', 'SDL3.dll', 'resources.pri')
foreach ($f in $required) {
    $src = Join-Path $TargetDir $f
    if (-not (Test-Path $src)) { throw "missing build artifact '$f' in $TargetDir (did 'cargo build --release' run?)" }
    Copy-Item $src (Join-Path $layout $f) -Force
}

# The VC++ runtime, app-local. Everything here links the DYNAMIC CRT (/MD): the Rust exes,
# SDL3.dll, and Skia — whose bundled HarfBuzz locks a std::mutex on every text shape. A
# msvcp140.dll older than 14.40 (VS 17.10's constexpr std::mutex) reads a null vtable on that
# lock, and the session dies with 0xC0000005 in MSVCP140.dll one second in. Shipping the
# toolset's own DLLs beside the exe wins the loader search over System32 on every machine.
$crt = Find-VcRedistCrt $Arch
$crtDlls = Get-ChildItem -Path $crt -File | Where-Object { $_.Name -match '^(msvcp140|vcruntime140)[^\\]*\.dll$' }
if ($crtDlls.Count -lt 3) { throw "expected msvcp140*.dll + vcruntime140*.dll in $crt, found $($crtDlls.Count)" }
foreach ($d in $crtDlls) { Copy-Item $d.FullName (Join-Path $layout $d.Name) -Force }
Write-Host "VC++ runtime from $crt : $(($crtDlls | ForEach-Object Name) -join ', ')"

# license/attribution payload (MSIX has no installer EULA page, so ship them as files): the
# project's own MIT/Apache texts plus the generated third-party notices, which is where every
# vendored/statically-linked dependency's attribution lives (openh264 BSD-2, rav1d BSD-2, …).
#
# The FFmpeg LGPL notice + license texts that used to be copied here went with the DLLs at M10:
# nothing in this package links libav* any more, so shipping an LGPL notice would be claiming a
# dependency that is not there.
#
# For the same reason the notices come from clients/windows/ and NOT from the repo root: the root
# file is workspace-wide, so it attributes crates this package never links. The client-scoped file
# (same generator, `--packages
# punktfunk-client-windows,punktfunk-client-session,punktfunk-cli`) is the one that describes what
# is actually inside this .msix — and it is the same file the app's Licenses page shows.
$licDir = Join-Path $layout 'licenses'
New-Item -ItemType Directory -Force -Path $licDir | Out-Null
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..')).Path
$clientRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
foreach ($n in @('LICENSE-MIT', 'LICENSE-APACHE')) {
    $p = Join-Path $repoRoot $n
    if (Test-Path $p) { Copy-Item $p $licDir -Force }
}
$notices = Join-Path $clientRoot 'THIRD-PARTY-NOTICES.txt'
if (-not (Test-Path $notices)) {
    throw "missing $notices — run scripts/gen-third-party-notices.sh (it generates the per-client copies)"
}
Copy-Item $notices $licDir -Force

# tile/store assets — and the Lucide icon font, which the shell loads by the ms-appx URI in
# app/lucide.rs. It is checked in beside the tile art rather than staged by the build, so this
# copy is the ONLY route it takes into a shipped package.
Copy-Item (Join-Path $assets '*') (Join-Path $layout 'Assets') -Force
# Assert it landed: without the font every icon in the shell renders as a private-use box, and
# nothing else in this script would notice.
$font = Join-Path $layout 'Assets\lucide.ttf'
if (-not (Test-Path $font)) { throw "missing Assets\lucide.ttf in the layout — the shell's icon font (see clients/windows/src/app/lucide.rs)" }

# manifest with version + publisher + architecture substituted
$manifest = (Get-Content -Raw $manifestTemplate).Replace('{VERSION}', $Version).Replace('{PUBLISHER}', $Publisher).Replace('{ARCH}', $Arch)
# The ARM64 session is built without the console (no Skia for the target): no tile for it.
if ($Arch -eq 'arm64') {
    $manifest = [regex]::Replace($manifest, '(?s)\s*<!-- console:begin -->.*?<!-- console:end -->', '')
    if ($manifest -match 'PunktfunkConsole') { throw 'the console entry survived the arm64 strip' }
}
Set-Content -Path (Join-Path $layout 'AppxManifest.xml') -Value $manifest -Encoding UTF8

# --- resource index (resources.pri) ---
# The shell resolves the manifest's logo assets through MRT, so the qualified variants
# (Square44x44Logo.targetsize-*_altform-unplated.png — the alpha-transparent taskbar icons) only
# take effect if a pri indexes them; without one the taskbar falls back to plating the base
# 44x44 onto a solid square (the white-cornered icon). makepri's default config indexes the
# layout's asset files AND merges any existing .pri it finds (reactor's staged WinUI resources)
# via its PRI indexer, yielding one combined resources.pri. Output lands outside the layout
# first — the reactor pri is an input while indexing — then replaces it.
$makepri = Find-SdkTool 'makepri.exe'
$priconfig = Join-Path $OutDir 'priconfig.xml'
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
& $makepri createconfig /cf $priconfig /dq en-US /o
if ($LASTEXITCODE -ne 0) { throw "makepri createconfig failed ($LASTEXITCODE)" }
$priOut = Join-Path $OutDir 'resources.pri'
if (Test-Path $priOut) { Remove-Item $priOut -Force }
& $makepri new /pr $layout /cf $priconfig /mn (Join-Path $layout 'AppxManifest.xml') /of $priOut /o
if ($LASTEXITCODE -ne 0) { throw "makepri new failed ($LASTEXITCODE)" }
Move-Item $priOut (Join-Path $layout 'resources.pri') -Force

Write-Host "layout assembled at $layout :"
Get-ChildItem $layout -Recurse -File | ForEach-Object { "  $($_.FullName.Substring($layout.Length + 1))" }

# --- pack ---
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$msix = Join-Path $OutDir "punktfunk-client-windows_${Version}_${Arch}.msix"
& $makeappx pack /o /d $layout /p $msix
if ($LASTEXITCODE -ne 0) { throw "makeappx pack failed ($LASTEXITCODE)" }

# --- sign (signing.ps1: Azure, then MSIX_CERT_PFX_B64, then ephemeral) ---
# The .pfx modes export the .cer users import once (Trusted People); a stable .pfx keeps that import
# good across upgrades. Azure emits none, so MSIX_CER_PATH stays unset.
$cerPath = Join-Path $OutDir "punktfunk-client-windows_${Version}_${Arch}.cer"
$signing = Resolve-SigningMode -OutDir $OutDir -Publisher $Publisher -FriendlyName 'punktfunk MSIX (self-signed)' `
    -PfxBase64 $PfxBase64 -PfxPassword $PfxPassword -AzureEndpoint $AzureEndpoint -AzureAccount $AzureAccount `
    -AzureProfile $AzureProfile -AzureDlib $AzureDlib -RequireSignedCert $RequireSignedCert -CerPath $cerPath
Sign-File $signing $msix -Package
Remove-SigningPfx
Remove-Item $signing.Metadata -Force -ErrorAction SilentlyContinue

# Read the signature back off the packed .msix and hold it against the manifest Publisher. MSIX
# package identity is Name + Publisher, so a publisher that doesn't match the signer isn't cosmetic:
# Add-AppxPackage refuses the package outright. Checking the ACTUAL signer (rather than a pfx we
# happen to hold) is the only form of this check that works in every signing mode, and failing the
# build here is the difference between a red pipeline and a release nobody can install.
# Deliberately asymmetric: a subject we CAN read and that DISAGREES is a hard failure, but a subject
# we cannot read at all is only a warning. Get-AuthenticodeSignature's support for the .msix/.appx
# subject interface varies by Windows version, and signtool has already reported success by this
# point — turning "the check could not run" into a build break would trade a real defect we catch for
# an imaginary one we invent.
$signerSubject = $null
try { $signerSubject = (Get-AuthenticodeSignature $msix).SignerCertificate.Subject } catch { }
if (-not $signerSubject) {
    Write-Warning ("could not read a signer subject back from $msix, so Publisher/signer agreement is " +
                   "UNVERIFIED on this box. If the package is rejected at Add-AppxPackage time, compare " +
                   "`signtool verify /pa /v` against the manifest Publisher '$Publisher' by hand.")
} elseif ($signerSubject -ne $Publisher) {
    throw ("signer subject does not match the manifest Publisher, so this package cannot install:`n" +
           "  signer    : '$signerSubject'`n" +
           "  Publisher : '$Publisher'`n" +
           "Pass -Publisher '$signerSubject' (or fix the certificate profile) and repack.")
} else {
    Write-Host "verified signer subject matches manifest Publisher: $signerSubject"
}

Write-Host ""
Write-Host "==> MSIX: $msix"
if ($signing.Mode -eq 'azure') {
    Write-Host "==> signed by a publicly trusted CA — nothing for users to import."
} else {
    Write-Host "==> trust the cert once per machine (then it stays trusted across all future builds):"
    Write-Host "    Import-Certificate -FilePath '$cerPath' -CertStoreLocation Cert:\LocalMachine\TrustedPeople"
}
# emit paths for the workflow to publish (only under CI, where GITHUB_ENV is set)
if ($env:GITHUB_ENV) {
    "MSIX_PATH=$msix" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
    if ($signing.Mode -ne 'azure') { "MSIX_CER_PATH=$cerPath" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8 }
}
