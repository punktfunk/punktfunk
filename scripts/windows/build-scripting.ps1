<#
  Rebuild the plugin/script runner bundle from the CURRENT sdk/ source and lay it out next to the
  host exe, so `punktfunk-host plugins ...` works and the PunktfunkScripting task runs new code.

    powershell -ExecutionPolicy Bypass -File scripts\windows\build-scripting.ps1
    powershell -ExecutionPolicy Bypass -File scripts\windows\build-scripting.ps1 -EnableTask

  WHY the layout matters: the plugins CLI forwards package ops (add/remove/list) to the runner, and
  on Windows it resolves the runner RELATIVE TO THE RUNNING EXE - <exe-dir>\bun\bun.exe and
  <exe-dir>\scripting\runner-cli.js (runner_command in
  crates\host\punktfunk-host\src\plugin_host\plugins\windows.rs). deploy-host.ps1 runs the
  service out of target\release, so a bundle sitting only in the installed {app} leaves the freshly
  built exe reporting "the plugin runner isn't installed". We therefore deploy next to EVERY host exe
  we can find: the built one, and whatever the PunktfunkHost service actually runs.

  The PunktfunkScripting task ships DISABLED (opt-in). We do not silently enable it - pass
  -EnableTask on a dev box you are validating plugins on.
#>
param(
    [switch]$EnableTask                              # enable + restart PunktfunkScripting (opt-in)
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path (Split-Path $PSScriptRoot)        # scripts\windows -> repo root
$sdk  = Join-Path $repo 'sdk'
$task = 'PunktfunkScripting'

# bun is both the bundler AND the runtime the runner ships on. Honor $env:BUN_EXE (what CI sets)
# before falling back to the dev box's portable copy - the same one build-web.ps1 uses.
$bun = $env:BUN_EXE
if (-not ($bun -and (Test-Path $bun))) { $bun = 'C:\Users\Public\bun\bin\bun.exe' }
if (-not (Test-Path $bun)) { throw "bun not found at $bun (set `$env:BUN_EXE to override)" }

Write-Host "== punktfunk plugin/script runner deploy =="
Write-Host "bun    : $bun"

# --- 1. build the self-contained bundle ------------------------------------------------------
# --target=bun inlines effect + the SDK into ONE js; the operator's plugin import stays a runtime
# import. --ignore-scripts skips sdk's `prepare` script (bun2nix, which rewrites the committed
# bun.nix).
$stage = Join-Path $repo 'target\scripting'
New-Item -ItemType Directory -Force -Path $stage | Out-Null
$bundle = Join-Path $stage 'runner-cli.js'

Push-Location $sdk
try {
    Write-Host "bun install ..."
    & $bun install --frozen-lockfile --ignore-scripts
    if ($LASTEXITCODE -ne 0) { throw "sdk bun install failed (exit $LASTEXITCODE)" }
    Write-Host "bun build src/runner-cli.ts -> $bundle"
    & $bun build src/runner-cli.ts --target=bun "--outfile=$bundle"
    if ($LASTEXITCODE -ne 0) { throw "runner bundle build failed (exit $LASTEXITCODE)" }
} finally { Pop-Location }

# Same gate CI and the .deb builder use: 'attempt=' only survives when the dynamic plugin import was
# bundled as a RUNTIME import. If it is missing the bundle would load but never run a plugin.
if (-not (Select-String -Path $bundle -Pattern 'attempt=' -Quiet)) {
    throw "runner bundle missing the dynamic plugin import - wrong build"
}
Write-Host "bundle : OK ($([math]::Round((Get-Item $bundle).Length / 1KB)) KB)"

# --- 2. work out every host-exe dir that needs the payload ------------------------------------
$targets = New-Object System.Collections.Generic.List[string]
function Add-Target([string]$exe) {
    if (-not $exe) { return }
    $dir = Split-Path $exe
    if ($dir -and (Test-Path $dir) -and -not ($targets -contains $dir)) { $targets.Add($dir) | Out-Null }
}

# a) the binary deploy-host.ps1 just built - what you invoke by hand to test the new CLI.
Add-Target (Join-Path $repo 'target\release\punktfunk-host.exe')

# b) whatever the service actually runs (an installed {app} on a box set up via setup.exe). The
#    binPath carries args and may be quoted, so pull the .exe out with a regex.
$qc = & sc.exe qc 'PunktfunkHost' 2>$null
if ($qc) {
    $line = $qc | Select-String 'BINARY_PATH_NAME' | Select-Object -First 1
    if ($line -and "$line" -match '([A-Za-z]:\\[^"]*?punktfunk-host\.exe)') { Add-Target $Matches[1] }
}

if ($targets.Count -eq 0) { throw "no punktfunk-host.exe found - run deploy-host.ps1 first." }

# --- 3. lay out <exe-dir>\scripting\{runner-cli.js,scripting-run.cmd} + <exe-dir>\bun\bun.exe ---
# Stop a LIVE runner first: it holds bun.exe (and the bundle) open, so copying over them fails with a
# sharing violation. Step 4 restarts it. Reap stragglers by command line the way build-web.ps1 does -
# schtasks /end does not always take the child bun with it.
$existing = Get-ScheduledTask -TaskName $task -ErrorAction SilentlyContinue
if ($existing -and $existing.State -eq 'Running') {
    Write-Host "stopping $task (holds bun.exe open) ..."
    & schtasks /end /tn $task 2>$null | Out-Null
    Get-CimInstance Win32_Process -Filter "Name='bun.exe'" -ErrorAction SilentlyContinue |
      Where-Object { $_.CommandLine -match 'runner-cli\.js' } |
      ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
    Start-Sleep 2
}

foreach ($dir in $targets) {
    Write-Host ""
    Write-Host "deploying -> $dir"
    $scrDir = Join-Path $dir 'scripting'
    $bunDir = Join-Path $dir 'bun'
    New-Item -ItemType Directory -Force -Path $scrDir, $bunDir | Out-Null

    Copy-Item $bundle (Join-Path $scrDir 'runner-cli.js') -Force
    Copy-Item (Join-Path $PSScriptRoot 'scripting-run.cmd') (Join-Path $scrDir 'scripting-run.cmd') -Force
    # The runner import()s the operator's .ts plugins, so bun ships WITH it rather than being assumed
    # on PATH - exactly what the installer does. Skip the copy when it is already the same build.
    $bunDst = Join-Path $bunDir 'bun.exe'
    if (-not (Test-Path $bunDst) -or (Get-Item $bunDst).Length -ne (Get-Item $bun).Length) {
        Copy-Item $bun $bunDst -Force
    }
    Write-Host "  scripting\runner-cli.js, scripting\scripting-run.cmd, bun\bun.exe"
}

# --- 3.5 de-privilege: LocalService principal + secret read grants ----------------------------
# The runner task runs as NT AUTHORITY\LocalService (NOT SYSTEM - a plugin defect must cost a
# throwaway account). Converge a task registered as SYSTEM by an older installer or by hand, and
# grant LocalService read on the scoped plugin-token and the TLS-pin cert.pem. NEVER mgmt-token
# (full admin). A subset of what `punktfunk-host plugins enable` grants
# (plugin_host/plugins/windows.rs).
if ($existing) {
    $principal = New-ScheduledTaskPrincipal -UserId 'LocalService' -LogonType ServiceAccount
    Set-ScheduledTask -TaskName $task -Principal $principal | Out-Null
    Write-Host ""
    Write-Host "task   : $task principal -> NT AUTHORITY\LocalService"
    $cfg = Join-Path $env:ProgramData 'punktfunk'
    foreach ($secret in @('plugin-token', 'cert.pem')) {
        $file = Join-Path $cfg $secret
        if (Test-Path $file) {
            & "$env:SystemRoot\System32\icacls.exe" $file /grant:r '*S-1-5-19:(R)' | Out-Null
            if ($LASTEXITCODE -ne 0) { Write-Host "warn   : icacls grant failed on $file" }
        } else {
            Write-Host "note   : $file not found - start the host once, then re-run (the runner"
            Write-Host "         needs LocalService read on it to authenticate)."
        }
    }
    # Unit dirs get inheritable (RX,WA): bun's module loader opens unit files requesting
    # FILE_WRITE_ATTRIBUTES on top of read - plain (RX) makes every import EPERM. WA can only
    # touch timestamps/attribute bits, never content.
    foreach ($unitDir in @('plugins', 'scripts')) {
        $dirPath = Join-Path $cfg $unitDir
        New-Item -ItemType Directory -Force -Path $dirPath | Out-Null
        & "$env:SystemRoot\System32\icacls.exe" $dirPath /grant:r '*S-1-5-19:(OI)(CI)(RX,WA)' | Out-Null
        if ($LASTEXITCODE -ne 0) { Write-Host "warn   : icacls grant failed on $dirPath" }
    }
    # State root gets inheritable Modify - the ONE writable grant, so a plugin can persist its
    # config/cache under plugin-state\<name> (@punktfunk/host's pluginStateDir). Code dirs stay
    # RX+WA, secrets stay R; only this dir is writable.
    $stateDir = Join-Path $cfg 'plugin-state'
    New-Item -ItemType Directory -Force -Path $stateDir | Out-Null
    & "$env:SystemRoot\System32\icacls.exe" $stateDir /grant:r '*S-1-5-19:(OI)(CI)(M)' | Out-Null
    if ($LASTEXITCODE -ne 0) { Write-Host "warn   : icacls grant failed on $stateDir" }
    # Ingest inbox gets inheritable Modify for BUILTIN\Users - the INVERSE grant, so an
    # interactive-user app (the Playnite exporter) can drop ingest\<plugin>\ data the LocalService
    # runner reads. The one Users-writable carve-out in the otherwise Users-read-only tree.
    $ingestDir = Join-Path $cfg 'ingest'
    New-Item -ItemType Directory -Force -Path $ingestDir | Out-Null
    & "$env:SystemRoot\System32\icacls.exe" $ingestDir /grant:r '*S-1-5-32-545:(OI)(CI)(M)' | Out-Null
    if ($LASTEXITCODE -ne 0) { Write-Host "warn   : icacls grant failed on $ingestDir" }
}

# --- 4. the opt-in scheduled task -------------------------------------------------------------
# Registered DISABLED by the installer; the runner is inert until there is automation to run. We only
# bounce it when it already exists, and only enable it when explicitly asked. ($existing was captured
# in step 3, BEFORE we stopped a running instance - so State still reflects how we found the box.)
Write-Host ""
if (-not $existing) {
    Write-Host "note   : the $task task is not registered on this box (installer registers it)."
    Write-Host "         The plugins CLI still works - it runs the runner directly."
}
elseif ($EnableTask) {
    if ($existing.State -eq 'Disabled') {
        Enable-ScheduledTask -TaskName $task | Out-Null
        Write-Host "task   : $task ENABLED (-EnableTask)"
    }
    & schtasks /end /tn $task 2>$null | Out-Null
    Start-Sleep 2
    & schtasks /run /tn $task | Out-Null
    Write-Host "task   : $task restarted on the new bundle"
}
elseif ($existing.State -eq 'Disabled') {
    Write-Host "note   : $task is DISABLED (opt-in, as shipped) - the new bundle is staged but no"
    Write-Host "         runner is supervising plugins. Enable it for on-glass validation with:"
    Write-Host "           powershell -File scripts\windows\build-scripting.ps1 -EnableTask"
    Write-Host "         (or: punktfunk-host plugins enable)"
}
else {
    & schtasks /end /tn $task 2>$null | Out-Null
    Start-Sleep 2
    & schtasks /run /tn $task | Out-Null
    Write-Host "task   : $task restarted on the new bundle"
}

Write-Host ""
Write-Host "DONE - plugin/script runner deployed."
