# Emit the winget manifests for a released Windows installer: the host, or the client with -Package.
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
# The client's two architectures come from windows-client.yml: -Package PunktfunkClient and
# -Arm64InstallerPath fill the template's `Architecture: arm64` entry.
#
# Emits: <OutDir>/unom.<Package>.yaml
#        <OutDir>/unom.<Package>.installer.yaml
#        <OutDir>/unom.<Package>.locale.en-US.yaml
#        <OutDir>/unom.<Package>.locale.<locale>.yaml for every other locale template
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Version,
    # The packed setup.exe — hashed here so the manifest can never disagree with what shipped.
    [Parameter(Mandatory = $true)][string]$InstallerPath,
    # Hashed the same way; the template's `Architecture: arm64` entry takes it.
    [string]$Arm64InstallerPath,
    [Parameter(Mandatory = $true)][string]$OutDir,
    [string]$Package = 'PunktfunkHost',
    [string]$TemplateDir = (Join-Path $PSScriptRoot '..\..\packaging\winget'),
    # Overridable so a fork/mirror can retarget without editing the templates.
    [string]$UrlBase = 'https://git.unom.io/unom/punktfunk/releases/download'
)

$ErrorActionPreference = 'Stop'

$id = "unom.$Package"
$TemplateDir = (Resolve-Path $TemplateDir).Path

# One hash and URL per architecture; each Installers entry takes its own.
$entries = @{}
foreach ($a in @(@('x64', $InstallerPath), @('arm64', $Arm64InstallerPath))) {
    if (-not $a[1]) { continue }
    if (-not (Test-Path $a[1])) { throw "installer not found: $($a[1])" }
    # winget's own tooling emits UPPERCASE hex; match it so a manifest diff against a wingetcreate-
    # generated one is empty rather than a case-only churn.
    $entries[$a[0]] = @{
        Sha = (Get-FileHash -Path $a[1] -Algorithm SHA256).Hash.ToUpperInvariant()
        Url = "$UrlBase/v$Version/$(Split-Path $a[1] -Leaf)"
    }
    Write-Host "winget manifest: version=$Version $($a[0]) sha256=$($entries[$a[0]].Sha)"
    Write-Host "                 url=$($entries[$a[0]].Url)"
}

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

# PackageVersion appears in every file; InstallerUrl/Sha256 only in the installer manifest.
# Anchored replacements (line-leading key) so a version string inside prose — e.g. the release-notes
# URL in the locale manifest — is never rewritten by accident.
$default = "$id.locale.en-US.yaml"
$locales = Get-ChildItem -Path $TemplateDir -Filter "$id.locale.*.yaml" |
    Where-Object Name -ne $default | Sort-Object Name | ForEach-Object Name
$files = @("$id.yaml", "$id.installer.yaml", $default) + @($locales)
foreach ($f in $files) {
    $src = Join-Path $TemplateDir $f
    if (-not (Test-Path $src)) { throw "template missing: $src" }
    $text = Get-Content -Path $src -Raw

    $text = [regex]::Replace($text, '(?m)^PackageVersion:.*$', "PackageVersion: $Version")
    # Split before each `- Architecture:` so an entry is rewritten with its own architecture's file.
    $text = -join ([regex]::Split($text, '(?m)^(?=[ \t]*- Architecture:)') | ForEach-Object {
        $arch = if ($_ -match '(?m)^\s*- Architecture: (\S+)') { $Matches[1] } else { 'x64' }
        $e = $entries[$arch]
        if (-not $e) { throw "$f lists $arch, which has no installer" }
        $part = [regex]::Replace($_, '(?m)^(\s*)InstallerUrl:.*$', "`${1}InstallerUrl: $($e.Url)")
        [regex]::Replace($part, '(?m)^(\s*)InstallerSha256:.*$', "`${1}InstallerSha256: $($e.Sha)")
    })
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
$inst = Get-Content -Path (Join-Path $OutDir "$id.installer.yaml") -Raw
foreach ($needle in @($Version) + @($entries.Values | ForEach-Object { $_.Sha, $_.Url })) {
    if ($inst -notmatch [regex]::Escape($needle)) { throw "substitution failed - '$needle' missing from the installer manifest" }
}
Write-Host "winget manifests ready in $OutDir"
