<#
  Rebuild the web mgmt console from the CURRENT web/ source and swap it into an installed host.

    powershell -ExecutionPolicy Bypass -File scripts\windows\build-web.ps1

  bun is both the build tool AND the runtime: vite.config's Nitro noExternals bundles every dep
  into the self-contained .output (no node_modules, nothing for bun to fail to resolve). The
  console runs as a supervised child of the PunktfunkHost service (bun {app}\web\.output\server\
  index.mjs on :47992), so the swap is: stop the service (its kill-on-close job takes the console's
  bun down and unlocks the files), replace {app}\web\.output, start the service - the supervisor
  brings the console back by itself. Needs an elevated shell (service control + Program Files).
#>
$ErrorActionPreference = 'Stop'
$repo = Split-Path (Split-Path $PSScriptRoot)
$web  = Join-Path $repo 'web'
$bun  = 'C:\Users\Public\bun\bin\bun.exe'
$app  = 'C:\Program Files\punktfunk'
if (-not (Test-Path $bun)) { throw "bun not found at $bun" }

Set-Location $web
Write-Host "bun install + build ..."
& $bun install
& $bun run build
if ($LASTEXITCODE -ne 0) { throw "web build failed (exit $LASTEXITCODE)" }
# No .output/server install step: noExternals means the output has no externalized deps to resolve.

$appWeb = Join-Path $app 'web'
if (-not (Test-Path $appWeb)) {
  Write-Host "no installed console at $appWeb - built web\.output only (run it by hand: bun web\.output\server\index.mjs)"
  return
}

Write-Host "swapping $appWeb\.output (stopping the PunktfunkHost service) ..."
$dst = Join-Path $appWeb '.output'
& net stop PunktfunkHost | Out-Null
try {
  # The removal MUST succeed before the copy. A merge of two builds is not a degraded
  # install, it is a dead one: Nitro's entry.mjs imports its siblings by content hash, so a
  # stale chunks/_ next to a new entry.mjs makes every page 200 with a bun ResolveMessage
  # body instead of the app.
  Remove-Item $dst -Recurse -Force -ErrorAction SilentlyContinue
  if (Test-Path $dst) {
    throw "could not remove $dst (files still locked - is another bun/host still running?). Refusing to copy over it: a mixed .output serves errors, not the console."
  }
  Copy-Item (Join-Path $web '.output') -Destination $appWeb -Recurse -Force
}
finally {
  & net start PunktfunkHost | Out-Null
}

# The console serves HTTPS-only (PUNKTFUNK_UI_SECURE=1, the host's own cert) - probe with curl.exe
# (-k for the self-signed cert; Invoke-WebRequest under Windows PowerShell 5.1, which this script
# runs under, has no -SkipCertificateCheck), retrying while the service/bun cold-starts.
#
# The BODY is the check, not the status code: a bun that started but cannot resolve its own
# chunks answers 200 with a ResolveMessage JSON, so a code-only probe reports a healthy
# console that serves nothing but an error.
$body = $null
for ($i = 0; $i -lt 15; $i++) {
  Start-Sleep 2
  $body = & curl.exe -sk --max-time 5 'https://127.0.0.1:47992/login' 2>$null
  if ($body -match '<html|<!DOCTYPE html') { break }
}
if ($body -match '<html|<!DOCTYPE html') {
  Write-Host "DONE - the console serves the app (/login returned HTML)"
} elseif ($body -match 'Cannot find module|ResolveMessage') {
  Write-Error "BROKEN - /login answered with a bun module-resolution error, i.e. .output is inconsistent: $body"
} else {
  Write-Warning "console swapped but /login did not serve HTML yet - check %ProgramData%\punktfunk\logs\web.log. Last body: $body"
}
