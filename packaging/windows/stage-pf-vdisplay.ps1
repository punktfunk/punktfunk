<#
.SYNOPSIS
  Stage the pf-vdisplay driver bundle the installer ships into -OutDir: the freshly-built signed
  pf-vdisplay driver (from -VendorDir) + the fetched nefcon device tool.

.DESCRIPTION
  pf-vdisplay (our all-Rust IddCx virtual display) is BUILT FROM SOURCE per release by
  build-pf-vdisplay.ps1 (packaging/windows/drivers/), which emits the signed pf_vdisplay.{dll,inf,cat}
  + punktfunk-driver.cer into a build-output dir. pack-host-installer.ps1 passes that dir here as
  -VendorDir; we copy it + add the nefcon tool. (The old checked-in binaries under
  packaging/windows/pf-vdisplay/ were retired - building from source keeps the .dll/.inf/.cat in
  lockstep; see design/windows-build-and-packaging.md.) nefcon publishes a pinned release, so we fetch +
  SHA-256-verify it (it provides nefconc.exe, used to create the root-enumerated device node - pnputil
  can't).

  Output (packed into the installer's staging tree): -OutDir gets pf_vdisplay.inf/.cat/.dll,
  pf_vdisplay_seats.inf/.cat + punktfunk-driver.cer and nefconc.exe (x64).

.EXAMPLE
  pwsh -File stage-pf-vdisplay.ps1 -OutDir C:\t\out\stage
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$OutDir,
    [Parameter(Mandatory = $true)][string]$VendorDir,   # the build-pf-vdisplay.ps1 output dir
    [ValidateSet('x64', 'arm64')][string]$Arch = 'x64', # picks the matching nefconc.exe from the zip
    # PINNED nefcon release (https://github.com/nefarius/nefcon/releases). MIT-licensed.
    [string]$NefconUrl = 'https://github.com/nefarius/nefcon/releases/download/v1.17.40/nefcon_v1.17.40.zip',
    [string]$NefconSha256 = '812bae7ed7dfb7d6d2284bc7de2f8ccebc92ed2a0b1ae893c53b337096e50c1a'
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$PSNativeCommandUseErrorActionPreference = $false

if (Test-Path $OutDir) { Remove-Item -Recurse -Force $OutDir }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

# --- built pf-vdisplay driver -----------------------------------------------------------------
$inf = Get-ChildItem -Path $VendorDir -Filter pf_vdisplay.inf -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $inf) { throw "no pf_vdisplay.inf under $VendorDir - did build-pf-vdisplay.ps1 run?" }
# The seats package rides along: the console installs it when the operator turns seats on.
foreach ($seats in 'pf_vdisplay_seats.inf', 'pf_vdisplay_seats.cat') {
    if (-not (Test-Path (Join-Path $VendorDir $seats))) { throw "no $seats under $VendorDir - did build-pf-vdisplay.ps1 run?" }
}
Copy-Item (Join-Path $VendorDir '*') $OutDir -Force
Write-Host "==> pf-vdisplay staged from $VendorDir"

# --- nefcon (fetched + verified) --------------------------------------------------------------
$work = Join-Path ([IO.Path]::GetTempPath()) ('nefcon-' + [IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Force -Path $work | Out-Null
try {
    $zip = Join-Path $work 'nefcon.zip'
    Write-Host "==> downloading $NefconUrl"
    Invoke-WebRequest -Uri $NefconUrl -OutFile $zip -UseBasicParsing
    $got = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($NefconSha256) {
        if ($got -ne $NefconSha256.ToLowerInvariant()) {
            throw "nefcon SHA-256 mismatch`n  expected $NefconSha256`n  got      $got"
        }
        Write-Host "    sha256 ok ($got)"
    }
    else { Write-Warning "no pinned nefcon SHA-256 - computed $got (PIN THIS in stage-pf-vdisplay.ps1)" }
    Expand-Archive -Path $zip -DestinationPath $work -Force
    $nefc = Get-ChildItem -Path $work -Recurse -Filter 'nefconc.exe' |
        Where-Object { $_.FullName -match "(?i)\\$Arch\\" } | Select-Object -First 1
    if (-not $nefc) { $nefc = Get-ChildItem -Path $work -Recurse -Filter 'nefconc.exe' | Select-Object -First 1 }
    if (-not $nefc) { throw "nefconc.exe not found in $NefconUrl" }
    Copy-Item $nefc.FullName (Join-Path $OutDir 'nefconc.exe') -Force
}
finally { Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue }

Write-Host "==> staged driver bundle in $OutDir :"
Get-ChildItem $OutDir -File | ForEach-Object { "    $($_.Name)" }
