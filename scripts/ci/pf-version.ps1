# Single source of truth for punktfunk release/canary version numbers (Windows runners).
#
# The pwsh twin of scripts/ci/pf-version.sh — see that file for the full rationale. Same rule:
#   * stable (vX.Y.Z tag) -> PF_BASE = X.Y.Z (minus any -rc/+meta suffix)
#   * canary (main push)  -> PF_BASE = <latest stable tag with minor+1, patch 0>  (always one ahead)
#
# Returns a hashtable AND (when $env:GITHUB_ENV is set) appends the same keys there for later
# steps. Usage in a pwsh workflow step:
#     $pf = & "$env:GITHUB_WORKSPACE/scripts/ci/pf-version.ps1"
#     $base = $pf.PF_BASE          # e.g. "0.7.0"
#     # numeric-version channels (MSIX/host installer) build "<major>.<minor>.<run>":
#     $v = "$($pf.PF_MAJOR).$($pf.PF_MINOR).$env:GITHUB_RUN_NUMBER"
# `-SelfTest` checks the keys it emits against scripts/ci/pf-version.vectors, shared with the bash
# twin. Run it in its own process: it rewrites GITHUB_REF and friends.
param([switch]$SelfTest)
$ErrorActionPreference = 'Stop'
# A non-zero native-command exit (e.g. a `git fetch` with no network) must NOT abort — the
# Cargo.toml fallback below covers it. On PS 7.4+ this pref would otherwise throw under -Stop.
$PSNativeCommandUseErrorActionPreference = $false

$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path

if ($SelfTest) {
  $fail = 0
  foreach ($line in Get-Content (Join-Path $PSScriptRoot 'pf-version.vectors')) {
    if ($line -match '^\s*(#|$)') { continue }
    $tags, $ref, $fmt, $want = ($line -split '\|').Trim()
    $env:PF_VERSION_TAGS = $tags
    $env:GITHUB_REF = $ref
    $env:GITHUB_REF_NAME = $ref -replace '^refs/[^/]+/', ''
    $env:GITHUB_ENV = $null
    $got = & $PSCommandPath
    foreach ($kv in -split $want) {
      $k, $v = $kv -split '=', 2
      if ($got.Contains($k) -and $got[$k] -ne $v) {
        Write-Host "pf-version.ps1: $ref ${fmt}: want $kv, got $k=$($got[$k])"
        $fail = 1
      }
    }
  }
  if ($fail -eq 0) { Write-Host 'pf-version.ps1: every vector matches' }
  exit $fail
}

if ($null -ne $env:PF_VERSION_TAGS) {
  $tags = -split $env:PF_VERSION_TAGS   # -SelfTest's tag list
} else {
  # checkout is shallow + tagless by default — canary needs the tag list. Best-effort.
  try { git -C $root fetch --tags --force --quiet 2>$null } catch { }
  $tags = git -C $root tag -l 'v*' 2>$null
}

$stable = $tags |
  ForEach-Object { if ($_ -match '^v(\d+\.\d+\.\d+)$') { $Matches[1] } } |
  Sort-Object { [version]$_ } |
  Select-Object -Last 1
if (-not $stable) {
  $stable = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "(\d+\.\d+\.\d+)"' |
    Select-Object -First 1).Matches.Groups[1].Value
}
if (-not $stable) { $stable = '0.0.0' }

if ($env:GITHUB_REF -like 'refs/tags/v*') {
  $channel = 'stable'
  $base = (($env:GITHUB_REF_NAME -replace '^v', '') -replace '[-+].*$', '')
} else {
  $channel = 'canary'
  $p = $stable.Split('.')
  $base = "$($p[0]).$([int]$p[1] + 1).0"
}

$b = $base.Split('.')
$out = [ordered]@{
  PF_CHANNEL    = $channel
  PF_BASE       = $base
  PF_MAJOR      = $b[0]
  PF_MINOR      = $b[1]
  PF_PATCH      = $b[2]
  PF_STABLE_TAG = $stable
  # Canary: workspace crates on 16 codegen units. A release tag keeps Cargo.toml's 1.
  CARGO_PROFILE_RELEASE_CODEGEN_UNITS = $(if ($channel -eq 'canary') { '16' } else { '1' })
}
foreach ($k in $out.Keys) {
  Write-Output ("{0}={1}" -f $k, $out[$k]) | Out-Null
  if ($env:GITHUB_ENV) { "$k=$($out[$k])" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8 }
}
$out
