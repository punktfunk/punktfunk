# punktfunk Windows client — packaging

Three artifacts, packed from one assembled layout by
[`windows-client.yml`](../../../.gitea/workflows/windows-client.yml) to Gitea's generic package
registry — canary on every `main` push that touches the client, stable on `vX.Y.Z` tags.

| Artifact | Notes |
|---|---|
| `punktfunk-client-setup_<arch>.exe` | **The default download.** Per-user, no-UAC, into `%LOCALAPPDATA%\Programs\Punktfunk`. |
| `punktfunk-client-windows_<arch>-portable.zip` | The same signed file set, nothing registered. |
| `punktfunk-client-windows_<arch>.msix` | Kept for Microsoft Store compatibility. |

`pack-msix.ps1` assembles `layout/` and packs the MSIX; `pack-client-installer.ps1` consumes that
same layout for the installer and zip, signing the four exes individually — the MSIX only signs its
container.

**Why the installer exists at all.** The MSIX install shape breaks the top user-reported flows: the
exe lands under the ACL'd `C:\Program Files\WindowsApps`, which Steam's *Add a Non-Steam Game*
picker cannot browse, and alias/`shell:AppsFolder` activation defeats the Steam overlay's injection
and Big Picture launch. Steam must spawn the exe itself from a normal path. The installer re-creates
the MSIX manifest's declarative grants per-user: `punktfunk://` in HKCU Classes, Start shortcuts,
and `{app}` on the user PATH for the `punktfunk` CLI.

**Two architectures, one x64 runner.** `x86_64-pc-windows-msvc` builds natively and
`aarch64-pc-windows-msvc` cross-compiles, because the x64 MSVC toolset ships the ARM64 cross
compiler. Nothing in the package links FFmpeg, so neither arch needs a per-arch `FFMPEG_DIR` staged
on the runner. ARM64 builds `punktfunk-session` with `--no-default-features` (no Skia console UI)
until rust-skia ships aarch64-pc-windows-msvc prebuilts.

**No FFmpeg DLLs, and no FFmpeg notice.** The client decodes natively, so shipping the LGPL notice
would claim a dependency the package does not have. The *host* installer is unchanged —
`packaging/windows/pack-host-installer.ps1` still ships those DLLs for its encode path.

**Why an "unpackaged" WinUI app packages cleanly.** `main` calls `windows_reactor::bootstrap()`,
which runs `MddBootstrapInitialize2` with `OnPackageIdentity_NOOP`. Under MSIX package identity the
bootstrapper is a no-op and the runtime resolves from the manifest's `<PackageDependency>` on
`Microsoft.WindowsAppRuntime.2` instead. It is a full-trust Win32 app because it owns raw D3D11,
Win32 low-level input hooks, WASAPI and SDL3.

## Versioning

MSIX requires a strictly 4-part numeric version:

- `vX.Y.Z` tag → `X.Y.Z.0`. Any `-rc` or `+meta` suffix is dropped. Published to the stable `latest/`
  alias and attached to the Gitea Release.
- `main` push or `workflow_dispatch` → `X.<Y+1>.<run_number>.0` — the minor *after* the latest `v*`
  tag, per `scripts/ci/pf-version.ps1`, climbing by run number. `canary/` alias.

## Signing

`pack-msix.ps1` and `pack-client-installer.ps1` pick a backend through
`packaging/windows/signing.ps1`, in this order:

1. **Azure Artifact Signing** when `AZURE_CODESIGNING_ENDPOINT` / `_ACCOUNT` / `_PROFILE` are all set
   (the workflow sets them; they are not secret). Credentials are the `punktfunk-ci-signing` service
   principal, which holds only the *Artifact Signing Certificate Profile Signer* role scoped to the
   `unom-io` profile. Keys are HSM-backed and never leave Azure — no `.pfx`, no `.cer` emitted, and
   the chain is publicly trusted so there is nothing for a user to import.
2. **`MSIX_CERT_PFX_B64` / `MSIX_CERT_PASSWORD`** — the older self-signed `CN=unom` cert, public half
   checked in as [`punktfunk-codesign.cer`](punktfunk-codesign.cer).
3. An **ephemeral** self-signed cert, for forks and local builds with no secrets.

Modes 2 and 3 emit a `.cer` to import into `Cert:\LocalMachine\TrustedPeople` first. On a `v*` tag a
build with no real signing backend **fails closed** rather than shipping a throwaway.

Two things about Azure mode are easy to get wrong:

- **Timestamping is mandatory, not best-effort.** Azure mints a leaf cert per request that expires in
  about three days, so an untimestamped signature stops verifying within days of release. The script
  refuses to retry without one.
- **The manifest `Publisher` must equal the signer's subject exactly**, because MSIX package identity
  is Name + Publisher. After signing, the script reads the signature back off the `.msix` and fails
  on any drift. Changing it makes a *different* package — existing installs must be uninstalled, not
  upgraded.

## Building locally

On a Windows box with MSVC and the Windows SDK, after a release build:

```powershell
cargo build --release -p punktfunk-client-windows --target x86_64-pc-windows-msvc
pwsh -File clients/windows/packaging/pack-msix.ps1 `
  -Version 0.2.0.0 -TargetDir C:\t\x86_64-pc-windows-msvc\release -OutDir C:\t\msix

cargo build --release -p punktfunk-client-windows --target aarch64-pc-windows-msvc
pwsh -File clients/windows/packaging/pack-msix.ps1 `
  -Version 0.2.0.0 -Arch arm64 -TargetDir C:\t\aarch64-pc-windows-msvc\release -OutDir C:\t\msix
```

Pack, sign and `Add-AppxPackage` all work headless. The only step needing a real display is
*launching* the WinUI window.
