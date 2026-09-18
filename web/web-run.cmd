@echo off
rem punktfunk web console launcher - DEV convenience, run BY HAND (in-repo tree). On an installed
rem host the PunktfunkHost service supervises the console itself (service.rs "web console child" -
rem no scheduled task, no launcher script); a dev host running from target\release has no installed
rem web payload next to its exe, so this script is how you serve the in-repo web\.output against it.
rem It sources the host's mgmt bearer token + the console login password from %ProgramData%\punktfunk\,
rem points the /api proxy at the host's loopback HTTPS mgmt API, and serves the self-contained
rem (no-node_modules) Nitro console over HTTPS (HTTP/1.1 over TLS) on :47992 with the host's identity
rem cert. %~dp0 = <repo>\web\ . The console runs on bun (the Nitro `bun` preset + Bun.serve TLS
rem entry) - set BUN below to your bun.exe. Rebuild after a web change with `bun run build` in web\ .
setlocal EnableExtensions

set "PFDATA=%ProgramData%\punktfunk"
set "TOKENFILE=%PFDATA%\mgmt-token"
set "PWFILE=%PFDATA%\web-password"
set "CERTFILE=%PFDATA%\cert.pem"
set "KEYFILE=%PFDATA%\key.pem"

rem The host's `serve` writes the mgmt token + identity cert on first run. Until they exist the proxy
rem has no credential and no TLS material, so WAIT for them (mirrors the service supervisor's gate)
rem rather than silently serving plain HTTP. ~5 min at 2 s, then give up.
set /a PFWAITS=0
:pfwait
if exist "%TOKENFILE%" if exist "%CERTFILE%" goto pfready
if %PFWAITS% GEQ 150 (
  echo [punktfunk-web] gave up waiting for "%TOKENFILE%" + "%CERTFILE%" - is the punktfunk host running?
  exit /b 1
)
if %PFWAITS%==0 echo [punktfunk-web] waiting for the host to write the mgmt token + identity cert...
set /a PFWAITS+=1
ping -n 3 127.0.0.1 >nul 2>&1
goto pfwait
:pfready

rem Both files are single KEY=VALUE lines: PUNKTFUNK_MGMT_TOKEN=... and, in the password file,
rem PUNKTFUNK_UI_PASSWORD=... until the console replaces it with PUNKTFUNK_UI_PASSWORD_HASH=... .
rem Split on the first '=' and import each under whichever name it carries; the server strips the
rem quotes the hash is stored in. PUNKTFUNK_UI_PASSWORD_FILE is the file the console rewrites.
for /f "usebackq tokens=1* delims==" %%A in ("%TOKENFILE%") do set "%%A=%%B"
if exist "%PWFILE%" for /f "usebackq tokens=1* delims==" %%A in ("%PWFILE%") do set "%%A=%%B"
set "PUNKTFUNK_UI_PASSWORD_FILE=%PWFILE%"

rem Fixed deployment wiring (the Windows analogue of scripts/punktfunk-web.service).
set "PORT=47992"
rem No HOST line: the server listens on the local network unless PUNKTFUNK_UI_BIND says otherwise
rem (host.env on an installed box). Set it here to keep this dev console to this machine.
rem set "PUNKTFUNK_UI_BIND=127.0.0.1"
set "PUNKTFUNK_MGMT_URL=https://127.0.0.1:47990"
rem ...unless the host published a different one. `serve` writes mgmt-endpoint in the same single
rem KEY=VALUE form as the token above, carrying the port it ACTUALLY bound - so a host moved off
rem 47990 (PUNKTFUNK_MGMT_BIND, e.g. to share the box with a Sunshine fork whose web UI owns that
rem port) brings the console with it. Imported AFTER the default so it wins; absent on an older host,
rem and then the default above stands.
set "ENDPOINTFILE=%PFDATA%\mgmt-endpoint"
if exist "%ENDPOINTFILE%" for /f "usebackq tokens=1* delims==" %%A in ("%ENDPOINTFILE%") do set "%%A=%%B"
rem No NODE_TLS_REJECT_UNAUTHORIZED: the host's self-signed cert is accepted only for the loopback
rem proxy hop, scoped inside the proxy code (Bun per-request TLS), not process-wide.
rem Serve HTTPS (HTTP/1.1 over TLS) with the host's identity cert; mark the session cookie Secure.
rem These name the LEGACY pair; the server prefers native-cert.pem/native-key.pem beside them when
rem both exist (the identity split - web\nitro-entry\tls-paths.mjs). Don't "fix" them to the native
rem names: a host that never took the split has no native pair, and the fallback lives in there.
set "PUNKTFUNK_UI_TLS_CERT=%CERTFILE%"
set "PUNKTFUNK_UI_TLS_KEY=%KEYFILE%"
set "PUNKTFUNK_UI_SECURE=1"

rem Bun runtime (override BUN if yours lives elsewhere / is on PATH as just `bun`).
if not defined BUN set "BUN=bun.exe"
set "SERVER=%~dp0.output\server\index.mjs"
if not exist "%SERVER%" (
  echo [punktfunk-web] built server missing at "%SERVER%" - build it: cd web ^&^& bun run build
  exit /b 1
)
"%BUN%" "%SERVER%"
