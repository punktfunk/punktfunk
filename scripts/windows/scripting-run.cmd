@echo off
rem punktfunk plugin/script runner launcher - the action the PunktfunkScripting scheduled task runs.
rem
rem OPT-IN: the installer registers that task DISABLED (the runner is inert until you add automation).
rem Enable it once you have scripts/plugins:  Enable-ScheduledTask -TaskName PunktfunkScripting
rem
rem Lays out next to the installed payload: {app}\scripting\scripting-run.cmd + runner-cli.js and
rem {app}\bun\bun.exe (so %~dp0 = {app}\scripting\). The runner discovers the operator's units under
rem %ProgramData%\punktfunk\{scripts,plugins}; a plugin's connect() auto-wires to the host's SCOPED
rem plugin-token + identity cert in %ProgramData%\punktfunk\ (written by the host's `serve`). The
rem task runs as NT AUTHORITY\LocalService - `plugins enable` grants it read on exactly those two
rem files. No env editing.
setlocal EnableExtensions

set "BUN=%~dp0..\bun\bun.exe"
set "RUNNER=%~dp0runner-cli.js"

if not exist "%RUNNER%" (
  echo [punktfunk-scripting] runner bundle missing at "%RUNNER%".
  exit /b 1
)
if not exist "%BUN%" (
  echo [punktfunk-scripting] bundled bun missing at "%BUN%".
  exit /b 1
)

rem The runner import()s the operator's .ts plugin files, so it runs on the bundled bun. SIGTERM (task
rem End) interrupts the whole unit tree structurally so plugin finalizers run before exit.
rem
rem Its stdout/stderr go to a file: a scheduled task has no console, and the runner's other log door
rem (shipping lines to the host's Logs page) needs the very connection whose failure is what you'd
rem be trying to read about - a runner that can't reach the host was silent everywhere (field report
rem 2026-08-18: task Running, plugins installed, "no logs at all"). plugin-state is the one dir
rem `plugins enable` makes writable for LocalService; the file inherits Users-read from the config
rem dir, so `type` it from any prompt. One previous run is kept as runner.log.1. If the dir isn't
rem writable (task started before `plugins enable` ever ran), start unlogged rather than not at all.
rem ponytail: no size cap within one run - rotate on size if a chatty plugin ever fills a disk.
rem A seat's runner logs into its seat's config dir, the one the runner itself reads.
set "LOG=%ProgramData%\punktfunk\plugin-state\runner.log"
if defined PUNKTFUNK_CONFIG_DIR set "LOG=%PUNKTFUNK_CONFIG_DIR%\plugin-state\runner.log"
set "LOGGED="
if exist "%LOG%" move /y "%LOG%" "%LOG%.1" >nul 2>&1
copy /y nul "%LOG%" >nul 2>&1 && set "LOGGED=1"
if defined LOGGED (
  >> "%LOG%" echo [punktfunk-scripting] %DATE% %TIME% starting "%BUN%" "%RUNNER%" as %USERNAME%
  "%BUN%" "%RUNNER%" >> "%LOG%" 2>&1
) else (
  "%BUN%" "%RUNNER%"
)

rem 78 is the runner refusing to run a plugin outside the AppContainer it can't build
rem (SANDBOX_REFUSED_EXIT, sdk\src\runner.ts). The task restarts a failed run every minute, 999
rem times, and each would refuse again - so that one status ends the task cleanly instead. The
rem reason is in runner.log and on the console's Troubleshooting page.
if %ERRORLEVEL% EQU 78 exit /b 0
