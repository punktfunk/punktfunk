<#
.SYNOPSIS
  Write a version into a built punktfunk-host binary's version slot.

.DESCRIPTION
  The slot is crates/host/punktfunk-host/src/version.rs: a marker, then 64 NUL-padded bytes. CI
  stamps the finished binary instead of handing cargo a per-run version, which would recompile
  the host on every run. Run it before signing: it rewrites bytes inside the image.

.EXAMPLE
  pwsh -File stamp-version.ps1 -Path C:\t\x86_64-pc-windows-msvc\release\punktfunk-host.exe -Version 0.40.35051
#>
param(
    [Parameter(Mandatory = $true)][string]$Path,
    [Parameter(Mandatory = $true)][string]$Version
)
$ErrorActionPreference = 'Stop'

$marker = "pf-version-slot`0"
$cap = 64
$payload = [Text.Encoding]::UTF8.GetBytes($Version)
if ($payload.Length -ge $cap) { throw "version '$Version' does not fit the $cap-byte slot" }

$bytes = [IO.File]::ReadAllBytes($Path)
# Latin-1 maps each byte to one char, so a string search finds the byte offset.
$text = [Text.Encoding]::Latin1.GetString($bytes)
$at = $text.IndexOf($marker, [StringComparison]::Ordinal)
if ($at -lt 0) { throw "no version slot in $Path" }
if ($text.LastIndexOf($marker, [StringComparison]::Ordinal) -ne $at) { throw "more than one version slot in $Path" }

$start = $at + $marker.Length
[Array]::Clear($bytes, $start, $cap)
[Array]::Copy($payload, 0, $bytes, $start, $payload.Length)
[IO.File]::WriteAllBytes($Path, $bytes)
Write-Host "stamped $Path with version $Version"
