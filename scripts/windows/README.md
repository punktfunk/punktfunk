# Windows host build/deploy scripts

Helper scripts for the Windows host box (the RTX `.173` lab box, repo at
`C:\Users\Public\punktfunk-native`). Run them from the repo root in an **elevated** PowerShell.

## One-time: persist the build environment

```powershell
powershell -ExecutionPolicy Bypass -File scripts\windows\setup-build-env.ps1
```

Persists (Machine scope) the vars the host build needs (NVENC itself needs none — its entry
points are runtime-loaded from the driver's `nvEncodeAPI64.dll`):

| var | value | why |
| --- | --- | --- |
| `LIBCLANG_PATH` | `C:\Program Files\LLVM\bin` | bindgen (`libclang.dll`) |

No `FFMPEG_DIR`: nothing in punktfunk links libavcodec. The VS C++ toolchain is loaded
per-build via `vcvars64.bat` (auto-discovered with `vswhere`).

## Rebuild + redeploy the host service

```powershell
powershell -ExecutionPolicy Bypass -File scripts\windows\deploy-host.ps1
```

Stops `PunktfunkHost`, backs up the current binary (`punktfunk-host.exe.bak`), builds
`--release -p punktfunk-host --features nvenc` from the current source, then restarts the
service on the new binary — **with automatic rollback** if the build fails or the new binary
won't start. The service is down only for the build duration.

## Web management console

On an **installed** host (the `setup.exe`) the console is set up automatically — no manual steps.
The installer bundles the built (self-contained, no-`node_modules`) `.output` server + a portable
bun; the **`PunktfunkHost` service supervises the console as its own child** (`bun
{app}\web\.output\server\index.mjs` on `:47992`, session 0, restarted on any exit, stdout in
`%ProgramData%\punktfunk\logs\web.log`), starting it as soon as the host has written its mgmt token
+ identity cert. `punktfunk-host.exe web setup` provisions the rest: it opens inbound TCP 47992 and
writes the login password to `%ProgramData%\punktfunk\web-password` (ACL'd to Administrators +
SYSTEM). The mgmt bearer token it proxies with is the host's own
`%ProgramData%\punktfunk\mgmt-token`. Browse `https://<host-ip>:47992` and log in with the password
the installer shows on its final page. The console replaces that clear line with a salted argon2id
hash on the first sign-in, so a forgotten password is **reset, not read**: put a
`PUNKTFUNK_UI_PASSWORD=<your-password>` line back in `web-password` (elevated — the file is ACL'd)
and restart the service (`punktfunk-host service restart`). The next sign-in re-hashes it and logs
every other session out.

### Rebuild + restart the console (dev box)

```powershell
powershell -ExecutionPolicy Bypass -File scripts\windows\build-web.ps1
```

`bun install && bun run build` (Nitro `noExternals` -> a self-contained `.output`, no
`node_modules`/`.npmrc`), then swaps the fresh `.output` into `{app}\web` around a service
stop/start (the service's kill-on-close job is what unlocks the console's bun) and checks
`:47992/login`. Use this to iterate on the console against an installed host.

## Plugin/script runner

```powershell
powershell -ExecutionPolicy Bypass -File scripts\windows\build-scripting.ps1
powershell -ExecutionPolicy Bypass -File scripts\windows\build-scripting.ps1 -EnableTask
```

`bun install && bun build src/runner-cli.ts --target=bun` in `sdk\` -> one self-contained
`runner-cli.js` (effect + the SDK inlined; the operator's plugin `import()` stays a runtime import,
gated on the same `attempt=` check CI and the `.deb` builder use), then lays it out as
`<exe-dir>\scripting\runner-cli.js` + `scripting-run.cmd` with the bun runtime at `<exe-dir>\bun\bun.exe`.

**That layout is load-bearing.** `punktfunk-host plugins add/remove/list` forwards package ops to the
runner, and on Windows it resolves the runner *relative to the running exe* (`crates\punktfunk-host\src\plugin_host\plugins.rs`).
Since `deploy-host.ps1` runs the service out of `target\release`, a bundle sitting only in the
installed `{app}` leaves the freshly built exe reporting *"the plugin runner isn't installed"*. The
script deploys next to **every** host exe it finds - the built one and whatever the `PunktfunkHost`
service actually runs.

The `PunktfunkScripting` task is registered **disabled** (opt-in) by the installer, so the script
stages the bundle but does not silently enable it. Pass `-EnableTask` on a box you are validating
plugins on (equivalent to `punktfunk-host plugins enable`).

## Rebuild + redeploy everything

```powershell
powershell -ExecutionPolicy Bypass -File scripts\windows\deploy-all.ps1
powershell -ExecutionPolicy Bypass -File scripts\windows\deploy-all.ps1 -EnableScriptingTask
```

Thin wrapper: runs `deploy-host.ps1`, `build-web.ps1` then `build-scripting.ps1` in sequence — the
web console and plugin runner are **always** included, so the host binary and the runner bundle
never drift apart. If the host build/start fails, `deploy-host.ps1` rolls itself back and throws,
which stops this script before the later steps run.

## Typical flow after pulling new code

```powershell
git pull
powershell -ExecutionPolicy Bypass -File scripts\windows\deploy-all.ps1
```
