# winget manifests — Windows host

The reviewed source of truth for the `unom.PunktfunkHost` winget package. Everything except
`PackageVersion` / `InstallerUrl` / `InstallerSha256` / `ReleaseNotesUrl` is edited **here**;
`scripts/ci/winget-manifest.ps1` only substitutes those four per release, so the switches,
agreements and installation notes stay under normal code review.

| File | Purpose |
| --- | --- |
| `unom.PunktfunkHost.yaml` | Version manifest — ties the other two together. |
| `unom.PunktfunkHost.installer.yaml` | Installer type, scope, silent switches, `ProductCode`, URL + hash. |
| `unom.PunktfunkHost.locale.en-US.yaml` | User-facing metadata, `Agreements`, `InstallationNotes`. |

## Why these choices

- **`InstallerType: exe`, `Scope: machine`, `ElevationRequirement: elevatesSelf`.** The installer
  is punktfunk-setup-win (the self-contained engine exe), which speaks the Inno Setup silent
  dialect verbatim but is not an Inno binary. The host registers a SYSTEM service, installs
  drivers and opens firewall ports; the exe's `requireAdministrator` manifest raises its own UAC
  prompt. There is no per-user scope.
- **`ProductCode: {7C9E6A52-…}_is1`** — the ARP key keeps Inno's `<AppId>_is1` name forever. This
  is what correlates an installed host with the package for `winget list` / `winget upgrade`, and
  it is what lets an Inno-installed host upgrade onto the engine. **It must track `HOST_ARP_KEY` in
  `crates/install/punktfunk-setup/src/platform/windows/mod.rs`** — if that GUID ever changes, change it
  here too or upgrades silently stop being detected.
- **`interactive` is in `InstallModes`.** `winget install unom.PunktfunkHost --interactive` runs the
  full existing wizard: every task checkbox and the web-console password page.
  Nothing about the installer changes to support it.
- **No `/MERGETASKS` in the silent switches.** A silent install deliberately takes the *same* task
  defaults the wizard shows, so the product does not differ by install channel — a per-channel
  default is a support trap ("it works when I install it by hand"). The disclosures the wizard puts
  on screen are carried by `Agreements` instead, which winget shows *before* install and requires
  the user to accept.
- **`UpgradeBehavior: install`** — the engine upgrades in place (it follows `InstallLocation`).
  Uninstalling first would run the service + driver teardown between versions.

## Opting out of individual tasks

The Inno-dialect `/MERGETASKS` takes `!` prefixes to deselect a default-checked task. Use `--override`
(replaces winget's switches) rather than `--custom` (appends — you would end up with two
`/MERGETASKS` on one command line):

```powershell
winget install unom.PunktfunkHost --override "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP- /MERGETASKS=!gamestream"
```

Task names: `installdriver`, `installgamepad`, `installhdrlayer`,
`gamestream`, `allowpublicfw`, `startservice`, `trayicon`.

## Three installer behaviours that exist for this path

All three live in `crates/install/punktfunk-setup/src/platform/windows/` and also fix the plain
double-click upgrade path:

- **A competing host moves our port; it never aborts.** Sunshine/Apollo on the box means both want
  TCP 47990, so `coexist_steps` writes `PUNKTFUNK_MGMT_BIND=0.0.0.0:47991` and carries on. The Inno
  installer put up a modal and failed the install instead, which under `/VERYSILENT` was an
  unattended run blocking on an invisible dialog.
- **The GameStream flag is fresh-install-only.** On an upgrade the flag is omitted entirely, which
  `service install` reads as "keep the stored setting". Passing an explicit on/off would overwrite
  it — so a silent upgrade, where no wizard carries the old choice forward, would flip a user's
  GameStream setting with nothing on screen.
- **The Public-firewall flag is fresh-install-only too**, and `--allow-public-network` is tri-state
  (`=on` / `=off` / absent → keep the recorded choice, resolved from the `fw-allow-public` marker in
  `windows/service.rs`). This task is default-*unchecked*, so without the change a silent upgrade
  would have silently **revoked** a Public-network opt-in the user made once. The bare
  `--allow-public-network` form still means `on` for existing scripts; a malformed value is a hard
  error rather than a fall-through, since a typo'd opt-*out* must never resolve to "keep Public
  open".

## Release flow

`.gitea/workflows/windows-host.yml` runs on stable `v*` tags only, **after** the installer is
attached to the Gitea release — winget validates the URL and hash, so a manifest must never be
published ahead of its artifact:

```powershell
scripts/ci/winget-manifest.ps1 -Version 0.19.2 `
  -InstallerPath C:\t\out\punktfunk-host-setup-0.19.2.exe -OutDir C:\t\out\winget
```

The generated trio is attached to the same release. Canary builds are excluded: winget pins one
immutable artifact per version, so the rolling `canary/` alias has nothing it could point at.

## Validating a change

```powershell
winget validate --manifest packaging\winget
winget install --manifest packaging\winget          # local install from the manifest
```

For a throwaway check, `winget-pkgs`' `Tools\SandboxTest.ps1` runs a manifest in Windows Sandbox.
Note the host needs a real GPU and installs drivers, so a Sandbox run exercises the *manifest*
(download, hash, switches, ARP correlation) rather than a working stream.

## Publishing

Through **our own REST source** on unom-1 — see [`server/`](server/README.md). It sits alongside the
docs (3220) and flatpak (3230) services, behind the same edge Caddy; `windows-host.yml` rebuilds and
ships its catalogue on every stable tag, so releasing is one pipeline with no manual step.

```powershell
winget source add -n punktfunk https://winget.punktfunk.unom.io -t Microsoft.Rest   # elevated, once
winget install unom.PunktfunkHost
```

These manifests stay in winget-pkgs' own format rather than a bespoke one, so submitting upstream
later is a copy, not a rewrite. Two things would need attention on that path: the signing note
below, and `Agreements` being verified-developers-only in the community repo.

> **Signing.** The installer is signed with **Azure Artifact Signing** (account `unomsigning`,
> profile `unom-io`) — a publicly trusted CA, so there is no `.cer` for users to import. This
> removed the blocker on submitting to the community repo (`microsoft/winget-pkgs`), whose
> `Binary-Validation-Error` / `Validation-Defender-Error` checks require a publicly trusted cert;
> the remaining upstream obstacle is `Agreements` being verified-developers-only. Note that a
> trusted cert is not an instant SmartScreen bypass: reputation still accrues per publisher over
> downloads, it just now accrues to a named identity instead of being permanently unknown.
