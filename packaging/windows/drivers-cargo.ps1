<#
.SYNOPSIS
  Run one cargo command in the driver workspace through a short filesystem root.

.EXAMPLE
  ..\drivers-cargo.ps1 'clippy -p pf-vdisplay --all-targets -- -D warnings'
  The whole command is ONE quoted string; see the param note below for why.

.DESCRIPTION
  The driver's encoder deps (pf-encode-win's `pyrowave` and `qsv` features) build PyroWave and
  oneVPL from source with CMake, and CMake probes the toolchain by building a tiny MSBuild
  project. MSBuild writes .tlog paths under the CMake tree and cannot exceed MAX_PATH, so a deep
  checkout fails before anything compiles - the CI runner's root is ~78 characters, which is
  already most of the budget once the target dir is added.

  The target dir cannot move: wdk-build's find_top_level_cargo_manifest() walks UP from OUT_DIR
  for the first ancestor with a Cargo.lock and does not support a relocated CARGO_TARGET_DIR.
  So shorten the ROOT instead - subst a drive letter onto the repo and run cargo through it.

  It must be the repo root, not the driver workspace: the workspace's path deps (pf-encode-win,
  pf-frame) live in crates/ and resolve UPWARD, which a drive mapped at the workspace has no
  parent for. Falls back to running in place when no drive letter is free.
#>
# ONE STRING, not $args and not a param() with [Parameter()]. Both lose arguments to PowerShell's
# own binder: an attribute makes this an advanced function, so cargo's `-p` collides with
# -ProgressAction/-PipelineVariable; and $args silently swallows the first `--`, which turns
# `clippy ... -- -D warnings` into `-D warnings` for cargo itself ("unexpected argument '-D'").
# A single quoted string reaches us intact and only cargo parses it.
param([string]$CommandLine)
# @() is load-bearing: a one-word command line leaves a scalar, and splatting a string
# passes it one CHARACTER per argument - `build` reaches cargo as `b u i l d`.
$CargoArgs = @($CommandLine -split '\s+' | Where-Object { $_ -ne '' })

$ErrorActionPreference = 'Continue'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$drivers = Join-Path $repoRoot 'packaging\windows\drivers'
$rel = $drivers.Substring($repoRoot.Length).TrimStart('\')

# wdk-build needs the workspace's own target dir; a shared CARGO_TARGET_DIR hides the lock.
$prevTarget = $env:CARGO_TARGET_DIR
Remove-Item Env:\CARGO_TARGET_DIR -ErrorAction SilentlyContinue

# A cancelled job leaves its drive mapped to a deleted checkout. Freeing it keeps every run on X:,
# so the absolute paths a kept target and its CMake caches recorded stay valid.
foreach ($m in @(& subst)) {
    if ($m -match '^([A-Z]:)\\: => (.+)$' -and -not (Test-Path -LiteralPath $Matches[2])) { & subst $Matches[1] /D 2>&1 | Out-Null }
}

# Try each letter for real rather than asking Get-PSDrive which is free: subst mappings are
# per-logon-session, so another job's letter looks free here and then fails to map. Giving up
# on the first refusal is what silently drops the whole MAX_PATH defence.
$subst = $null
foreach ($letter in 'X', 'Y', 'W', 'V', 'U', 'T', 'S', 'R') {
    & subst "${letter}:" $repoRoot 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) { continue }
    if (Test-Path "${letter}:\$rel") { $subst = "${letter}:"; break }
    & subst "${letter}:" /D 2>&1 | Out-Null
}
$runDir = if ($subst) { "$subst\$rel" } else { $drivers }
# In place is fine from a short root and fatal from a deep one, and the failure lands minutes
# later as an MSBuild MSB3191 inside CMake. Say it here, where it is still legible.
if (-not $subst) {
    Write-Host "    (no drive letter would map - running in place from a $($repoRoot.Length)-char root)"
    if ($repoRoot.Length -gt 40) {
        Write-Host "    WARNING: that root leaves little of the 260-char budget; a CMake dep (pyrowave-sys, libvpl-sys) may fail with MSB3191."
    }
}

Write-Host "==> cargo $($CargoArgs -join ' ')  [in $runDir]"
Push-Location $runDir
& cargo @CargoArgs
$rc = $LASTEXITCODE
Pop-Location

if ($subst) { & subst $subst /D 2>&1 | Out-Null }
if ($prevTarget) { $env:CARGO_TARGET_DIR = $prevTarget }
exit $rc
