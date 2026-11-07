# Emit the winget manifests for a released Windows host installer.
#
# winget pins the installer by SHA256, so a manifest is only ever valid for ONE immutable artifact.
# The `latest/` channel alias in the generic registry is therefore NOT usable here — this always
# writes the versioned Gitea release URL, which the registry makes immutable (a re-upload 409s).
#
# The files are templated from packaging/winget/, which holds the reviewed, hand-maintained
# copy: everything except PackageVersion / InstallerUrl / InstallerSha256 is edited THERE, not here.
# That keeps the switches, agreements and installation notes under normal code review rather than
# buried in a generator.
#
# Usage (from windows-host.yml, stable tags only):
#     & scripts/ci/winget-manifest.ps1 -Version 0.19.2 -InstallerPath C:\t\out\punktfunk-host-setup-0.19.2.exe -OutDir C:\t\out\winget
#
# Emits: <OutDir>/unom.PunktfunkHost.yaml
#        <OutDir>/unom.PunktfunkHost.installer.yaml
#        <OutDir>/unom.PunktfunkHost.locale.en-US.yaml
#        <OutDir>/unom.PunktfunkHost.locale.<locale>.yaml for every other locale template
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Version,
    # The packed setup.exe — hashed here so the manifest can never disagree with what shipped.
    [Parameter(Mandatory = $true)][string]$InstallerPath,
    [Parameter(Mandatory = $true)][string]$OutDir,
    [string]$TemplateDir = (Join-Path $PSScriptRoot '..\..\packaging\winget'),
    # Overridable so a fork/mirror can retarget without editing the templates.
    [string]$UrlBase = 'https://git.unom.io/unom/punktfunk/releases/download'
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path $InstallerPath)) { throw "installer not found: $InstallerPath" }
$TemplateDir = (Resolve-Path $TemplateDir).Path

# winget's own tooling emits UPPERCASE hex; match it so a manifest diff against a wingetcreate-
# generated one is empty rather than a case-only churn.
$sha = (Get-FileHash -Path $InstallerPath -Algorithm SHA256).Hash.ToUpperInvariant()
$url = "$UrlBase/v$Version/$(Split-Path $InstallerPath -Leaf)"
Write-Host "winget manifest: version=$Version sha256=$sha"
Write-Host "                 url=$url"

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

# PackageVersion appears in every file; InstallerUrl/Sha256 only in the installer manifest.
# Anchored replacements (line-leading key) so a version string inside prose — e.g. the release-notes
# URL in the locale manifest — is never rewritten by accident.
$default = 'unom.PunktfunkHost.locale.en-US.yaml'
$locales = Get-ChildItem -Path $TemplateDir -Filter 'unom.PunktfunkHost.locale.*.yaml' |
    Where-Object Name -ne $default | Sort-Object Name | ForEach-Object Name
$files = @('unom.PunktfunkHost.yaml', 'unom.PunktfunkHost.installer.yaml', $default) + @($locales)
foreach ($f in $files) {
    $src = Join-Path $TemplateDir $f
    if (-not (Test-Path $src)) { throw "template missing: $src" }
    $text = Get-Content -Path $src -Raw

    $text = [regex]::Replace($text, '(?m)^PackageVersion:.*$', "PackageVersion: $Version")
    $text = [regex]::Replace($text, '(?m)^(\s*)InstallerUrl:.*$', "`${1}InstallerUrl: $url")
    $text = [regex]::Replace($text, '(?m)^(\s*)InstallerSha256:.*$', "`${1}InstallerSha256: $sha")
    # The release-notes link is per-version and lives outside the anchored keys above.
    $text = [regex]::Replace($text, '(?m)^ReleaseNotesUrl:.*$',
        "ReleaseNotesUrl: https://git.unom.io/unom/punktfunk/src/tag/v$Version/docs/releases/v$Version.md")

    $dest = Join-Path $OutDir $f
    # UTF-8 *without* BOM: winget's YAML parser rejects a BOM'd manifest.
    [IO.File]::WriteAllText($dest, $text, (New-Object Text.UTF8Encoding $false))
    Write-Host "  wrote $dest"
}

# Fail loudly if a template ever loses one of the fields we rewrite — a silently un-substituted
# manifest would publish the PREVIOUS release's hash, which winget would then refuse to install.
$inst = Get-Content -Path (Join-Path $OutDir 'unom.PunktfunkHost.installer.yaml') -Raw
foreach ($needle in @($Version, $sha, $url)) {
    if ($inst -notmatch [regex]::Escape($needle)) { throw "substitution failed - '$needle' missing from the installer manifest" }
}
Write-Host "winget manifests ready in $OutDir"
