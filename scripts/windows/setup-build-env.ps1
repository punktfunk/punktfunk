<#
  Persist the punktfunk Windows-host build environment (Machine scope, run once, elevated).

  After this, deploy-host.ps1 needs only the VS toolchain (it loads vcvars64.bat itself).
  These are BUILD-time vars; runtime config lives in C:\ProgramData\punktfunk\host.env.

    powershell -ExecutionPolicy Bypass -File scripts\windows\setup-build-env.ps1
#>
$ErrorActionPreference = 'Stop'

$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
         ).IsInRole([Security.Principal.WindowsBuiltinRole]::Administrator)
if (-not $admin) { throw "Run elevated (Machine-scope env requires Administrator)." }

# libclang for bindgen. (NVENC needs no build-time env:
# its entry points are runtime-loaded from the driver's nvEncodeAPI64.dll.)
$vars = [ordered]@{
  'LIBCLANG_PATH'               = 'C:\Program Files\LLVM\bin'
}
foreach ($k in $vars.Keys) {
  [Environment]::SetEnvironmentVariable($k, $vars[$k], 'Machine')
  Write-Host ("set (Machine)  {0} = {1}" -f $k, $vars[$k])
}
Write-Host "Done. Open a fresh shell (or reboot) for these to be inherited by new processes."
