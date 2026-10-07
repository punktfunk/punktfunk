<#
  Rebuild the punktfunk Windows host (release + NVENC) from the CURRENT source and
  restart the LocalSystem service, with automatic rollback if the build fails or the
  new binary won't start.

    powershell -ExecutionPolicy Bypass -File scripts\windows\deploy-host.ps1

  Prereqs: run setup-build-env.ps1 once (persists the build env), and VS C++ build tools
  installed (vcvars64.bat is auto-discovered via vswhere). The service is stopped for the
  duration of the build (the running .exe is locked), then restarted on the new binary.
#>
$ErrorActionPreference = 'Stop'
$repo = Split-Path (Split-Path $PSScriptRoot)        # scripts\windows -> repo root
$exe  = Join-Path $repo 'target\release\punktfunk-host.exe'
$bak  = "$exe.bak"
$svc  = 'PunktfunkHost'

function Find-VcVars {
  $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
  if (Test-Path $vswhere) {
    $ip = & $vswhere -latest -products * -property installationPath 2>$null
    if ($ip) { $v = Join-Path $ip 'VC\Auxiliary\Build\vcvars64.bat'; if (Test-Path $v) { return $v } }
  }
  $fb = 'C:\Program Files\Microsoft Visual Studio\18\Community\VC\Auxiliary\Build\vcvars64.bat'
  if (Test-Path $fb) { return $fb }
  throw "vcvars64.bat not found - install the VS C++ build tools."
}

function Svc-Running { (& sc.exe query $svc 2>$null) -match 'RUNNING' }

Write-Host "== punktfunk host deploy =="
$vcvars = Find-VcVars
Write-Host "vcvars : $vcvars"
Set-Location $repo

# Load the persisted build env (Machine scope) into THIS process, so the build sees it even
# if this shell was started before setup-build-env.ps1 ran (env is inherited at spawn time).
foreach ($k in 'LIBCLANG_PATH') {
  $v = [Environment]::GetEnvironmentVariable($k, 'Machine')
  if ($v) { [Environment]::SetEnvironmentVariable($k, $v, 'Process'); Write-Host "env    : $k=$v" }
  else { Write-Warning "env $k not set (run setup-build-env.ps1)" }
}

# All three vendors, no build-time tree: NVENC and AMF resolve their entry points from the driver
# at run time, and QSV's dispatcher is vendored. A build without `qsv` leaves an Intel GPU
# preference with no encode path, so the session dies at encoder open.
$features = 'nvenc,qsv'

# 1. stop the service so the .exe is writable
Write-Host "stopping $svc ..."
& sc.exe stop $svc | Out-Null
for ($i=0; $i -lt 30 -and (Svc-Running); $i++) { Start-Sleep 1 }

# 2. back up the current binary for rollback
if (Test-Path $exe) { Copy-Item $exe $bak -Force; Write-Host "backup : $bak" }

# 3. build (release); build env is inherited from Machine scope (setup-build-env.ps1)
Write-Host "building: cargo build --release -p punktfunk-host --features $features"
& cmd.exe /c "call `"$vcvars`" >nul && cargo build --release -p punktfunk-host --features $features"
$built = ($LASTEXITCODE -eq 0)

if (-not $built) {
  Write-Warning "BUILD FAILED (exit $LASTEXITCODE) - restoring previous binary."
  if (Test-Path $bak) { Copy-Item $bak $exe -Force }
  & sc.exe start $svc | Out-Null
  throw "build failed; previous binary restored and service restarted."
}

# 4. start on the new binary and confirm it stays up
Write-Host "build OK - starting $svc ..."
& sc.exe start $svc | Out-Null
$running = $false
for ($i=0; $i -lt 20; $i++) { if (Svc-Running) { $running = $true; break }; Start-Sleep 1 }

if (-not $running) {
  Write-Warning "new binary did not start - rolling back."
  & sc.exe stop $svc | Out-Null; Start-Sleep 2
  if (Test-Path $bak) { Copy-Item $bak $exe -Force }
  & sc.exe start $svc | Out-Null
  throw "new binary failed to start; rolled back to previous."
}

Write-Host "DONE - $svc running the new binary:"
Get-Item $exe | Select-Object LastWriteTime, Length | Format-List
