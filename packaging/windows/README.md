# Windows host packaging — the signed installer

A one-file, signed `setup.exe` for the punktfunk streaming **host** on Windows, published to Gitea's
generic package registry (`punktfunk-host-windows`) by `.gitea/workflows/windows-host.yml`.

> The installer is the `crates/punktfunk-setup-win` engine exe, packed by
> `pack-host-installer.ps1`. Inno Setup is gone; the silent flags, the ARP key and
> `unins000.exe` it froze are kept, so a fielded Inno install upgrades onto it unchanged.

> Toolchain, drivers-from-source and the dev loop in full: punktfunk-planning
> `windows-build-and-packaging.md`. This README is the `packaging/windows/` file index.

## Windows 11 22H2+ only (no Windows 10)

The installer refuses anything below **Windows 11 22H2 (build 22621)** — `MIN_HOST_BUILD` in
`crates/punktfunk-setup/src/platform/windows/plan.rs`, checked before the plan touches the box.
The floor comes from the
**pf-vdisplay** driver: it is built against the **IddCx 1.10** class extension (the HDR `*2` DDIs +
the FP16 adapter cap, linked via the 1.10 `IddCxStub`, no runtime `IddCxGetVersion` downgrade), and
IddCx 1.10 first shipped in Windows 11 22H2. On older Windows — **all of Windows 10 including LTSC,
and Windows 11 21H2** — the driver *package* installs fine, but the device then fails to start with
**Code 10 `STATUS_DEVICE_POWER_FAILURE`** in Device Manager and every session dies with "pf-vdisplay
driver interface not found". Gating the installer turns that late, confusing failure into an upfront
message. (Down-level SDR-only support would need a runtime IddCx version check in the driver —
tracked as a possible future feature, not planned.)

## ARM64 (Snapdragon X): built, unverified on hardware

`windows-host.yml` cross-builds a second leg for `aarch64-pc-windows-msvc` on the x64 runner (the
MSVC ARM64 cross compiler, the WDK's `um\arm64` libs) and publishes it as
`canary/punktfunk-host-setup_arm64.exe`. Every script in this directory takes `-Arch arm64`; the
driver workspace's `nvenc`/`qsv`/`pyrowave` features are x86-64-only in its `Cargo.toml`, so an
ARM64 `pf_vdisplay.dll` opens **Media Foundation and nothing else** (the Qualcomm MFT is the only
hardware encoder on Adreno). What that leg does not have, by architecture: NVENC, QSV, AMF,
PyroWave (Granite has no MSVC-ARM64 SIMD path).

**It streams video only.** Steam ships its streaming-audio drivers (`SteamStreaming{Speakers,
Microphone}.sys`) for x64 and x86, not arm64, and they are the host's whole audio substrate. The
runtime logs that and carries on. No Windows-on-ARM box has run this build yet; #298 keeps the
on-glass checklist (driver start, self-signed catalog acceptance, the MFT inside WUDFHost).

## Why not MSIX (like the client)

The host installs a **`LocalSystem` SCM service** that `CreateProcessAsUserW`'s from Session 0 into the
interactive session for secure-desktop (UAC / lock screen) capture, adds firewall rules, and depends
on the **pf-vdisplay** UMDF/IDD virtual-display driver. MSIX's sandbox can install **neither** a SYSTEM
service of this kind **nor** a driver. So the host ships as a classic elevated installer.

The installer is deliberately thin: the real install logic lives in `punktfunk-host` subcommands, not
in PowerShell — `service install` (SCM registration, firewall rules, the default `host.env`, the
SYSTEM→interactive-session supervisor; `service.rs`), `driver install [--gamepad]` and `web setup`
(driver/console provisioning; `windows/install.rs`). The installer lays the exe into
`C:\Program Files\punktfunk\` and calls those subcommands elevated. Keeping the logic in the compiled
exe — not a `.ps1` *file* PowerShell reads in the machine codepage — is the fix for the ANSI-codepage
parse breakage that silently failed installs on non-English boxes.

## What the installer does

The task list, the silent-install flags and the uninstall behaviour are on the
[docs site](https://docs.punktfunk.unom.io/docs/windows-host). Three decisions behind them belong
here, because nothing else records them:

- **Upgrades never overwrite `host.env`.** A default is written only if absent, and a hand-edited
  `PUNKTFUNK_HOST_CMD` survives. The one rewrite is `serve --gamestream` (or no line) to `serve`
  with the GameStream setting kept on. On an upgrade the GameStream task is inert (the flag is
  omitted), so change an installed host in the web console or with
  `punktfunk-host service install --gamestream=on|off`, not by re-running the wizard.
- **A driver failure warns, never aborts.** The host degrades to a physical display without
  pf-vdisplay, so a partial install is better than none.
- **A VB-CABLE from an older install is deliberately not removed.** It is a third-party shared
  component the user may rely on elsewhere; its own uninstaller is `VBCABLE_Setup_x64.exe -u -h`.
  `%ProgramData%\punktfunk` is left in place too, so a reinstall keeps the console password.

Wizard branding assets are generated **and committed** by `branding/gen-branding.ps1` from the
canonical brand geometry in `web/src/components/brand-mark.tsx`. Re-run it only on a brand change.

## Prerequisites on the target box

The CI exe is built `--features nvenc,qsv`; AMD uses native AMF. There is no software encoder:
Media Foundation is the fallback, within 8-bit 4:2:0. Virtual gamepads need no prerequisite — the DualSense / DualShock 4 / Xbox 360 UMDF
drivers are bundled and `pnputil`-installed, and ViGEmBus is no longer used.

**Audio is the exception, and it is structural.** A Windows audio device can only be created by a
**kernel-mode** driver — no UMDF path exists — so unlike our own drivers we cannot ship one. The
host instead mints its own devnode instances of Valve's vendor-signed streaming-audio drivers on the
target box. Audio therefore needs **Steam installed, never running**; the installer shows a
suppressible notice when it is absent and the host re-checks live, so installing Steam later just
works.

## Files here

| File | Role |
|------|------|
| `branding/` | `gen-branding.ps1` renders the brand mark into the committed `punktfunk.ico`. Re-run only on a brand change. |
| `pack-host-installer.ps1` | Orchestrator: cert + sign exe, **build + sign the drivers from source**, stage them + the **web console** (`.output` + bun) + the HDR layer, build + pack the wizard, sign setup.exe. |
| `build-pf-vdisplay.ps1` | Build pf-vdisplay from source (the `drivers/` workspace) + clear FORCE_INTEGRITY + sign `.dll`/`.cat` + export `.cer`. |
| `build-gamepad-drivers.ps1` | Sign + catalog the gamepad drivers (`pf-gamepad` + `pf-xusb`) from the same workspace build (`-SkipBuild`), one shared cert. |
| `make-driver-cert.ps1` | Generate the stable `CN=punktfunk-driver` code-signing cert (the `DRIVER_CERT_PFX_B64` / `DRIVER_CERT_PASSWORD` secrets). No key container, so it works over SSH; self-tests with signtool where it can. See *Driver signing* above. |
| `clear-force-integrity.ps1` | Clear the `/INTEGRITYCHECK` PE bit so a self-signed driver loads (reused by every driver build). |
| `stage-pf-vdisplay.ps1` | Stage the just-built pf-vdisplay bundle + fetch/verify the **pinned** nefcon release. |
| `drivers/` | The all-Rust IddCx **driver source** workspace: the `pf-vdisplay` crate on `wdk-sys` / windows-drivers-rs + the owned `pf-driver-proto` ABI + `wdk-iddcx`, plus `deploy-dev.ps1` (build/sign/install for dev). |
| `reset-pf-vdisplay.ps1` | **Dev:** recover a wedged driver — stop host → reap ghost monitor nodes → reload the adapter → start host (no reboot). See *Dev iteration* below. |
| `redeploy-pf-vdisplay.ps1` | **Dev:** one-shot redeploy — (optional) build → stop host → `deploy-dev.ps1 -Install` → reload adapter → start host. |
| `pf-vkhdr-layer/` | **HDR Vulkan layer** (standalone `cdylib`): lets Vulkan games (Doom: The Dark Ages, etc.) enable HDR over the virtual display by advertising the HDR surface formats the NVIDIA/AMD ICDs hide on an indirect display. Built by the packer, laid into `{app}\vklayer`, registered under `HKLM64\…\Khronos\Vulkan\ImplicitLayers` (opt-out *Install the HDR Vulkan layer* task). Self-gated on the display's HDR state. See its README. |

> **Drivers are built from source, not vendored.** All three (pf-vdisplay + the gamepad pf-gamepad /
> pf-xusb) are members of the all-Rust `drivers/` workspace (windows-drivers-rs / IddCx) and are
> **rebuilt + signed every release** by `build-pf-vdisplay.ps1` + `build-gamepad-drivers.ps1` - the
> checked-in prebuilt binaries were deleted (a stale `.cat` once stopped covering its `.inf` →
> `SPAPI_E_FILE_HASH_NOT_IN_CATALOG` on every box, and a frozen binary predated a driver IOCTL the host
> needed). Building from source keeps `.dll`/`.inf`/`.cat` in lockstep. nefcon (the device-node tool -
> the install creates the `root\pf_vdisplay` node with it, **never** `devgen`, which leaves persistent
> phantom devices) is fetched + SHA-256-verified from its pinned release in `stage-pf-vdisplay.ps1`. See
> punktfunk-planning: `windows-build-and-packaging.md` (internal planning repo) for the toolchain
> + signing details.

## Installer signing (Azure Artifact Signing)

`setup.exe`, `punktfunk-host.exe`, `punktfunk-tray.exe` and the Vulkan HDR layer are signed with
**Azure Artifact Signing** (formerly Trusted Signing): account `unomsigning`, certificate profile
`unom-io`, endpoint `https://neu.codesigning.azure.net/`. It is a publicly trusted CA, so users get
a named publisher in the UAC prompt and there is no `.cer` to import — `HOST_CER_PATH` is simply not
emitted in this mode (every consumer already guards on `Test-Path`).

`pack-host-installer.ps1` resolves a backend in this order, first match wins:

| order | backend | selected by |
| --- | --- | --- |
| 1 | Azure Artifact Signing | `AZURE_CODESIGNING_ENDPOINT` + `_ACCOUNT` + `_PROFILE` all set |
| 2 | stable self-signed `.pfx` | `MSIX_CERT_PFX_B64` / `MSIX_CERT_PASSWORD` |
| 3 | ephemeral self-signed | nothing set (canary / local only; a `v*` tag **fails closed**) |

Credentials for mode 1 come from the environment via `DefaultAzureCredential` — `AZURE_TENANT_ID`,
`AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`, the `punktfunk-ci-signing` service principal. It holds
exactly one role, *Artifact Signing Certificate Profile Signer*, scoped to the `unom-io` profile: it
can sign and can do nothing else with the subscription. The script hard-fails if the trio is missing
rather than letting `DefaultAzureCredential` fall through to an interactive login that would hang a
runner forever.

> **Timestamping is mandatory here, not best-effort.** Azure mints a leaf certificate per request,
> valid for about three days. An untimestamped signature therefore goes untrusted within days of
> release — it would verify fine on the runner and fail on users' machines that weekend. `Sign-File`
> refuses to retry without a timestamp in Azure mode; modes 2 and 3 keep the old lenient retry, where
> the cert outlives the release anyway.

### Runner setup

`signtool` reaches Azure through `Azure.CodeSigning.Dlib.dll`, which ships in the
`Microsoft.Trusted.Signing.Client` NuGet package — no installer, no fixed path. On the Windows runner:

```powershell
nuget install Microsoft.Trusted.Signing.Client -OutputDirectory $env:USERPROFILE\.nuget\packages
```

`Find-AzureDlib` searches that path and `C:\trusted-signing\`, newest first, so a package update needs
no script edit. Set `AZURE_CODESIGNING_DLIB` to override with an explicit path.

## Driver signing (`DRIVER_CERT_PFX_B64`)

> **The drivers are deliberately NOT on Azure.** Their catalogs keep the self-signed
> `CN=punktfunk-driver` cert below, which the installer still plants in the machine `Root` store.
> The two signatures are independent by design — Windows verifies the installer via SmartScreen/UAC
> and driver catalogs via PnP, and never requires a common signer, which is why the installer could
> move to a public CA without touching the driver track at all.
>
> Worth revisiting: these are **user-mode** (UMDF) drivers and we already clear `FORCE_INTEGRITY`, so
> a catalog signed by the publicly trusted Azure cert would likely chain to a root every Windows box
> already has — which would let us drop the `Root` plant entirely and keep only the `TrustedPublisher`
> entry that suppresses the device-software prompt. That is a real reduction in what we ask of a
> user's machine, but it is **unverified**: test it on the Windows box before believing it.

Our three UMDF drivers are signed with a **stable self-signed code-signing cert**, subject
`CN=punktfunk-driver`, supplied to `build-pf-vdisplay.ps1` / `build-gamepad-drivers.ps1` as the
`DRIVER_CERT_PFX_B64` + `DRIVER_CERT_PASSWORD` Actions secrets. On a `v*` tag build a missing cert
is a **hard failure** (`-RequireSignedCert`, default `auto` off `GITHUB_REF`); canary and local
builds still fall back to a per-build throwaway.

**Current fingerprint (SHA-1 thumbprint):** `4B8493E7CD565758D335F8F4F05C5A7261A13E02`

Verify a shipped driver against it:

```powershell
$dll = Get-ChildItem C:\Windows\System32\DriverStore\FileRepository\pf_vdisplay*\pf_vdisplay.dll |
         Select-Object -First 1 -Expand FullName
(Get-AuthenticodeSignature $dll).SignerCertificate.Thumbprint
```

Why stable matters here. The installer trusts the `.cer` that ships in the bundle
(`certutil -addstore -f Root` + `TrustedPublisher`, `crates/punktfunk-host/src/windows/install.rs`),
which is unavoidable for a self-signed cert — a self-signed leaf is its own root, so the chain only
validates if the root is present. That means the signature does **not** authenticate the download:
anyone who can alter the bundle can put their own cert next to their own driver. What a stable cert
buys is everything downstream of that: one anchor imported once instead of two more roots per
upgrade, a fingerprint we can publish out-of-band so a substituted driver is *detectable*, a
publisher an admin can allowlist, and continuity across releases. `driver install` purges stale
`CN=punktfunk-driver` certs before adding the current one, and `driver uninstall` removes them
entirely — including the pile left by the per-build-cert era.

> ⚠️ **The private key is now worth stealing.** It is trusted as a machine **root** on every
> punktfunk box, with code-signing EKU and no practical revocation path (nobody removes a stale
> root, and self-signed roots aren't in any CRL users honour). Keep it in the CI secret and nowhere
> else — not on a dev laptop. This is the trade for stability, and the reason attestation signing
> (which chains to Microsoft and needs no root import at all) remains the real fix.

Generating it — **run `make-driver-cert.ps1` yourself** on a Windows box; it prints the thumbprint
and writes the two secret values to files, and the private key never touches a certificate store:

```powershell
pwsh -File packaging\windows\make-driver-cert.ps1 -TestOnly   # dry run: generates, self-tests, keeps nothing
pwsh -File packaging\windows\make-driver-cert.ps1             # the real thing
```

Then add both values as **repo**-level Gitea Actions secrets on `unom/punktfunk` — same scope as
the `MSIX_CERT_PFX_B64` cert next door, and only this repo builds drivers (`RPM_GPG_PRIVATE_KEY` is
org-level because other repos publish RPMs; nothing else needs this one). Back up the `.pfx` and its
password somewhere you'd keep a signing key, then delete the output folder.

Two details the script exists to get right, both learned the hard way:

- It builds the cert with the .NET `CertificateRequest` API instead of `New-SelfSignedCertificate`,
  so **no key container is involved** and generation works over SSH. `New-SelfSignedCertificate`
  fails there with `NTE_PERM 0x80090010` — a network logon has no key container. Note that
  *consuming* a `.pfx` (signtool, or loading it in .NET) still needs one, which is why the script's
  signtool self-test reports SKIPPED over SSH rather than failing. The key is valid either way; run
  it at a console/RDP session to exercise the self-test, or let a canary build be the proof.
- The extension set is explicit and matches what the drivers have always been signed with —
  `KeyUsage=DigitalSignature` (critical), `EKU=codeSigning` (non-critical), SubjectKeyIdentifier,
  and deliberately **no** basicConstraints. This is not the place to improvise: a chain-building
  difference would surface as a failed driver install on a user's machine, not as a build error.

It also avoids `Get-Random` for the .pfx passphrase (that's `System.Random`, not a cryptographic
RNG) and uses .NET's own PKCS#12 writer rather than OpenSSL, whose 3.x default AES-256/PBKDF2
encryption produces a `.pfx` Windows CryptoAPI often cannot read.

Keep an offline backup of the .pfx + password somewhere you'd keep a signing key. Losing it means
the next release ships a cert nobody has trusted before, and every user's installer adds a second
root — recoverable, but only by re-running the install.

## Dev iteration on the test box (driver)

Two helpers wrap the painful manual steps of iterating on the pf-vdisplay driver against a live host
service. Run **elevated**; both default to the `PunktfunkHost` service. (The `C:\t-goal1\...` probe
path below is the maintainer's test box — substitute your own `punktfunk-probe.exe` build.)

```powershell
# Recover a WEDGED driver. Symptom: every session fails with
#   create virtual output: pf-vdisplay ADD ...: DeviceIoControl(0x222400): Element nicht gefunden (0x80070490)
# i.e. ERROR_NOT_FOUND — sustained ADD/REMOVE churn exhausted the IddCx monitor slots (ghost
# "Generic Monitor (Punktfunk)" nodes pile up, target_ids climb). A host restart's CLEAR_ALL does NOT
# fix it; the driver instance must be reloaded. This clears the ghosts + cycles the adapter (no reboot —
# this box boots to Proxmox).
powershell -ExecutionPolicy Bypass -File reset-pf-vdisplay.ps1 -Verify -Probe C:\t-goal1\debug\punktfunk-probe.exe

# Redeploy a driver build cleanly (stop host → install with a strictly-increasing DriverVer → reload
# adapter → start host). -Build runs `cargo build --release` first, but ONLY from an MSVC dev shell
# (LIBCLANG_PATH + Version_Number=10.0.26100.0); otherwise build separately and omit -Build.
powershell -ExecutionPolicy Bypass -File redeploy-pf-vdisplay.ps1 -Build -Verify -Probe C:\t-goal1\debug\punktfunk-probe.exe
```

The driver should reclaim monitor slots on REMOVE so churn can't wedge it; until it does, `reset` is
the recovery. From a Linux box drive either over SSH, e.g.
`ssh user@box 'powershell -ExecutionPolicy Bypass -File C:\...\reset-pf-vdisplay.ps1'`.

## Build locally (Windows, MSVC + Windows SDK)

```powershell
# 1. build the host (NVENC needs no import lib — its entry points are runtime-loaded; `qsv`
#    statically links the vendored VPL dispatcher — needs cmake + a libclang)
cargo build --release -p punktfunk-host --features nvenc,qsv

# 2. pack (self-signed unless the AZURE_CODESIGNING_* trio or MSIX_CERT_PFX_B64/MSIX_CERT_PASSWORD
#    are set — see "Installer signing" above; -NoDriver to skip pf-vdisplay)
pwsh -File packaging\windows\pack-host-installer.ps1 -Version 0.0.0-dev -TargetDir C:\t\release -OutDir C:\t\out
```

## Release

Push a `vX.Y.Z` tag — one tag releases every platform (see
[Release Channels](https://punktfunk.unom.io/docs/channels)). The workflow builds, signs, and
publishes `punktfunk-host-setup-X.Y.Z.exe` (no `.cer` — Azure signing is publicly trusted, and mode 2
or 3 would be needed to emit one), refreshes the stable `latest/`
alias, and attaches the installer to the unified Gitea Release. Main pushes publish rolling
`<next-minor>.<run>` **canary** builds (base derived from the latest stable tag by
`scripts/ci/pf-version.ps1`) to the `canary/` alias.
