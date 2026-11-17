# Layers punktfunk-specific tooling onto the shared unom Windows CI runner: the
# aarch64-pc-windows-msvc rustup target (windows-client.yml's ARM64 leg). The runner itself -
# act_runner, Node, rustup, VS Build Tools/NASM/CMake/LLVM - is provisioned generically by
# unom/infra (windows-runner/windows-runner.pkr.hcl + proxmox/windows-runner's Terraform clone);
# this script is what punktfunk adds on top, since the ARM64 target isn't every project's
# concern. See also provision-windows-wdk.ps1 for the driver-build toolchain (also punktfunk-only).
#
# Idempotent - safe to re-run. Run ELEVATED (admin) on the runner.
[CmdletBinding()]
param()
$ErrorActionPreference = "Stop"
function info($m) { Write-Host "[provision-punktfunk-extras] $m" }

$env:RUSTUP_HOME = "C:\Users\Public\.rustup"
$env:CARGO_HOME  = "C:\Users\Public\.cargo"

# --- ARM64 cross-compile target (windows-client.yml builds aarch64-pc-windows-msvc off
# this x64 box; the ARM64 MSVC cross compiler itself comes from unom/infra's generic VS Build
# Tools provisioning, which already includes the ARM64 component). ---
$rustup = "C:\Users\Public\.cargo\bin\rustup.exe"
if (Test-Path $rustup) {
  info "rustup target add aarch64-pc-windows-msvc"
  & $rustup target add aarch64-pc-windows-msvc
  # The x86 build of pf_seat_audio.dll, for 32-bit processes in a seat (pack-host-installer.ps1).
  info "rustup target add i686-pc-windows-msvc"
  & $rustup target add i686-pc-windows-msvc
} else {
  Write-Warning "rustup not found at $rustup - has unom/infra's setup-gitea-runner-base.ps1 run on this box yet?"
}

# No FFmpeg here: nothing in punktfunk links libav*. A stale C:\Users\Public\ffmpeg can be deleted
# by hand; this script does not remove what it no longer installs.


# --- No Vulkan-Headers here: nothing punktfunk builds on Windows needs Vulkan headers at compile
# time - ash generates its own bindings and dlopens vulkan-1.dll, which is a GPU-driver component.
# A stale C:\Users\Public\vulkan-headers is harmless; delete it by hand if you want the disk back. ---

# --- No Inno Setup here: the installers are packed by punktfunk-setup-win, which is a workspace
# crate and needs nothing on the runner. A leftover Inno Setup 6 install is harmless; uninstall it
# by hand if you want the disk back. ---

# No VB-CABLE here: the installer bundles no cable - the host mints its audio endpoints from
# Steam's streaming drivers on the target box. A stale C:\Users\Public\vbcable on a runner is
# harmless and can be deleted.

# --- The runner daemon's punktfunk env extension point (unom/infra's
# scripts/setup-gitea-runner-base.ps1) stays empty: punktfunk needs no runner env vars. Clear a
# file an older run wrote. ---
$projectEnv = "C:\Users\Public\act-runner\project-env.ps1"
if (Test-Path $projectEnv) {
  Set-Content -Encoding UTF8 $projectEnv "# punktfunk needs no runner env vars"
  info "cleared $projectEnv - restart the gitea-act-runner scheduled task to pick it up"
}

# --- Azure Artifact Signing (formerly Trusted Signing) toolchain, for the signing step in
# windows-host.yml + windows-client.yml. Two pieces, neither of which the generic unom/infra image
# carries, and both of which fail in ways that do not name themselves:
#
#   1. The .NET 8 runtime. Azure.CodeSigning.Dlib.dll is a mixed-mode (C++/CLI) assembly - it ships
#      Ijwhost.dll and a runtimeconfig.json pinning Microsoft.NETCore.App 8.0.0 - so on a box with
#      no .NET runtime, signtool exits 3 having printed NOTHING AT ALL. pwsh 7 is self-contained
#      and brings no shared runtime.
#   2. The signing client, installed MACHINE-WIDE under C:\trusted-signing rather than into a user's
#      .nuget. The act_runner daemon runs as SYSTEM, whose USERPROFILE is
#      C:\Windows\System32\config\systemprofile - so a per-user install under Administrator is
#      invisible to every job that actually builds. Find-AzureDlib (packaging/windows/signing.ps1)
#      searches this exact path for that reason.
#
# Both are SHA-256 pinned against version-immutable URLs (a nuget.org flat-container package and the
# dotnet builds CDN are both immutable per version), so these fail closed on tampering rather than
# every time Microsoft ships a patch release. Bump version + hash together to move either. ---
$dotnetVer = '8.0.30'
$dotnetSha = 'E40F199C6D5584AFF0554C01163C3C8D9CCF6BEC3A577E4D967E41070772A1C1'
$tscVer = '1.0.95'
$tscSha = '3BFCF1E0A3CB42AF1692F0A8ED45C15DE070C2DE86F28A59B2795D904D8A920F'

if (Test-Path 'C:\Program Files\dotnet\shared\Microsoft.NETCore.App') {
  info "shared .NET runtime already present ($((Get-ChildItem 'C:\Program Files\dotnet\shared\Microsoft.NETCore.App' | ForEach-Object Name) -join ', '))"
} else {
  info "installing .NET $dotnetVer runtime (required by Azure.CodeSigning.Dlib.dll)"
  $dn = "$env:TEMP\dotnet-runtime-$dotnetVer-win-x64.exe"
  Invoke-WebRequest -Uri "https://builds.dotnet.microsoft.com/dotnet/Runtime/$dotnetVer/dotnet-runtime-$dotnetVer-win-x64.exe" -OutFile $dn -UseBasicParsing
  $got = (Get-FileHash $dn -Algorithm SHA256).Hash
  if ($got -ne $dotnetSha) { Remove-Item $dn -Force; throw ".NET runtime download hash mismatch (got $got, pinned $dotnetSha)." }
  # -Wait is load-bearing: the bundle is a GUI PE that returns immediately when invoked with &,
  # leaving $LASTEXITCODE unset and racing any completion check against the install.
  $p = Start-Process -FilePath $dn -ArgumentList '/install', '/quiet', '/norestart' -Wait -PassThru
  Remove-Item $dn -Force -ErrorAction SilentlyContinue
  if ($p.ExitCode -ne 0) { throw ".NET runtime installer exited $($p.ExitCode)." }
  if (-not (Test-Path 'C:\Program Files\dotnet\shared\Microsoft.NETCore.App')) { throw ".NET runtime installer reported success but installed no shared runtime." }
}

$tscDir = "C:\trusted-signing\microsoft.trusted.signing.client\$tscVer"
if (Test-Path (Join-Path $tscDir 'bin\x64\Azure.CodeSigning.Dlib.dll')) {
  info "Trusted Signing client $tscVer already present at $tscDir"
} else {
  info "installing Microsoft.Trusted.Signing.Client $tscVer (machine-wide, for SYSTEM)"
  $nupkg = "$env:TEMP\microsoft.trusted.signing.client.$tscVer.nupkg"
  Invoke-WebRequest -Uri "https://api.nuget.org/v3-flatcontainer/microsoft.trusted.signing.client/$tscVer/microsoft.trusted.signing.client.$tscVer.nupkg" -OutFile $nupkg -UseBasicParsing
  $got = (Get-FileHash $nupkg -Algorithm SHA256).Hash
  if ($got -ne $tscSha) { Remove-Item $nupkg -Force; throw "Trusted Signing client download hash mismatch (got $got, pinned $tscSha)." }
  if (Test-Path $tscDir) { Remove-Item -Recurse -Force $tscDir }
  New-Item -ItemType Directory -Force -Path $tscDir | Out-Null
  Add-Type -AssemblyName System.IO.Compression.FileSystem
  [System.IO.Compression.ZipFile]::ExtractToDirectory($nupkg, $tscDir)
  Remove-Item $nupkg -Force -ErrorAction SilentlyContinue
  Get-ChildItem -Path $tscDir -Recurse -File | Unblock-File -ErrorAction SilentlyContinue
  if (-not (Test-Path (Join-Path $tscDir 'bin\x64\Azure.CodeSigning.Dlib.dll'))) { throw "extracted $tscVer but bin\x64\Azure.CodeSigning.Dlib.dll is absent." }
}

info "punktfunk extras provisioned OK."
