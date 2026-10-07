# punktfunk on Bazzite — packaging and install paths

The packager/ops view of the Bazzite (Fedora Atomic) target: which install path to ship, how each
one is built and updated, and the gotchas of layered/bootc hosts. **Setting a host up** — udev and
the `input` group, `host.env`, the service, the firewall, streaming the KDE desktop, verifying —
is the docs' job: follow **[docs.punktfunk.unom.io/docs/bazzite](https://docs.punktfunk.unom.io/docs/bazzite)**
(firewall specifics are on [Fedora §4](https://docs.punktfunk.unom.io/docs/fedora), same firewalld
service definitions). Nothing here duplicates those pages. For the higher-level packaging
rationale ("why not Flatpak", the build), see [`../README.md`](../README.md).

> ⚠️ **COPR note (Path C only).** The legacy layering path's commands reference a COPR project
> named `enricobuehler/punktfunk` that is operator-run and may not be published (see
> `packaging/copr/README.md`); layer from the **Gitea RPM registry** instead (`../rpm/README.md`,
> the repo file `https://git.unom.io/api/packages/unom/rpm/bazzite.repo`) — it's what CI
> actually publishes to. Paths A (sysext) and B (bootc) don't involve the COPR at all.

## 1. Choose an install path

There are three paths on Bazzite, driven by different files in `packaging/`:

| Path | Driven by | What it does | Best for |
|---|---|---|---|
| **A — systemd-sysext** ✅ recommended | `packaging/bazzite/punktfunk-sysext.sh` + `build-sysext.sh` (published by `.gitea/workflows/rpm.yml`) | Overlays the host onto `/usr` as a system extension — no layering, no reboot, one-command updates | Everyone; the default |
| **B — bootc / OCI image** | `packaging/bootc/Containerfile` | Bakes punktfunk into a `FROM bazzite-nvidia` image once; you `bootc switch` any number of hosts onto it | Fleets, reproducible appliances, no per-host drift |
| **C — rpm-ostree layering** (legacy) | `packaging/rpm/` + the Gitea RPM registry | Layers the `punktfunk` RPM onto your deployment with `rpm-ostree install` | Only if you specifically want the RPM database to own the files |

**Why A over C:** the Bazzite docs treat layering as a last resort — every layered package makes
every OS update slower and can **block upgrades entirely** until removed. A sysext never enters an
rpm-ostree transaction: it merges/unmerges at runtime, survives OS updates, and updating punktfunk
is one command with **no reboot** (layering needs one per update). It's the mechanism the Fedora
Atomic maintainers ship via [fedora-sysexts](https://fedora-sysexts.github.io/).

### Path A — systemd-sysext (recommended)

Install, day-2 commands, channel switching, rollback, feed-signature refusals and the
major-rebase behavior are all on the [docs page](https://docs.punktfunk.unom.io/docs/bazzite) —
that's the walkthrough to hand a user. Packager-side facts:

- CI (`.gitea/workflows/rpm.yml`) builds the image per Fedora major and publishes it to the feed
  `…/packages/unom/generic/punktfunk-sysext/f<ver>[-canary]/` — `SHA256SUMS` plus a detached
  OpenPGP signature from `packages@unom.io` (`AF245C506F4E4763`, the RPM signing key). The public
  key is baked into `punktfunk-sysext.sh`; the script refuses a feed it can't verify
  (`PUNKTFUNK_SYSEXT_ALLOW_UNSIGNED=1` is the documented escape hatch for pre-signing feeds).
- `SHA256SUMS` opens with `# FEED <name>` and `# SERIAL <unix-ts>` **inside the signed bytes**, so
  the signature says which feed and which publish it covers — a write:package token without the
  signing key can otherwise copy a canary manifest into the stable path, or put last month's back,
  and every box verifies it happily. `punktfunk-sysext` refuses a manifest that is unbound, is
  bound to a different feed, or whose serial is below the highest it has accepted (persisted per
  feed in `/var/lib/extensions/.punktfunk.serial-floor`, which survives `remove` on purpose).
  `publish-sysext-feed.sh` stamps both on every publish; a feed published before binding existed
  is bound by its next publish, or now with
  `TOKEN=… bash packaging/bazzite/publish-sysext-feed.sh --seal f<ver>[-canary]`.
- The image embeds `ID=fedora` + `VERSION_ID` (matched through Bazzite's `ID_LIKE`), so after a
  major rebase the old image is refused instead of merging soname-broken binaries; feeds exist
  per Fedora major, from the same CI matrix as the RPM groups.
- SELinux labels are baked into the image at build time (squashfs pseudo-xattrs computed from the
  targeted policy) — without them udev couldn't read the gamepad rule under enforcing. Validated
  live on Bazzite 43.
- Install also applies what the RPM scriptlets would have (udev reload, sysctl) and seeds the
  `/etc` file a sysext can't carry (the gamescope-session drop-in), staged under
  `/usr/share/punktfunk/etc/`.

### Path B — bootc image (`FROM bazzite-nvidia`)

The image is built **off-host** (on any machine with `podman`) from
`packaging/bootc/Containerfile`, which bases on `ghcr.io/ublue-os/bazzite-nvidia:stable`
(override with `--build-arg BASE_IMAGE=…`), enables RPM Fusion free + nonfree, adds the Gitea RPM
repo (`--build-arg PUNKTFUNK_RPM_GROUP=…`, default `bazzite`), and installs the host **and the web
console** (`punktfunk punktfunk-web`). It uses the Gitea registry rather than the COPR specifically
because the registry carries `punktfunk-web` (COPR's mock chroot can't build it — no `bun`).

```sh
# Build + push (run from the repo root, on your builder machine):
podman build -t ghcr.io/<you>/bazzite-punktfunk -f packaging/bootc/Containerfile .
podman push  ghcr.io/<you>/bazzite-punktfunk

# On each target Bazzite host:
sudo bootc switch ghcr.io/<you>/bazzite-punktfunk && systemctl reboot
```

> ⚠️ The image installs from the **Gitea RPM registry** (group `bazzite`), so **Path B depends on
> that registry being populated** — CI (`.gitea/workflows/rpm.yml`) publishes `punktfunk` +
> `punktfunk-web` on every push to `main`. Packages are unsigned with GPG-signed metadata
> (`repo_gpgcheck=1`), matching `packaging/rpm/README.md`.

### Path C — rpm-ostree layering (legacy)

Run on the Bazzite host. (Commands verbatim from `packaging/README.md`.)

```sh
# 1. RPM Fusion (free + nonfree) — provides the freeworld VAAPI drivers.
#    Usually already enabled on Bazzite; harmless to re-run.
rpm-ostree install \
  https://mirrors.rpmfusion.org/free/fedora/rpmfusion-free-release-$(rpm -E %fedora).noarch.rpm \
  https://mirrors.rpmfusion.org/nonfree/fedora/rpmfusion-nonfree-release-$(rpm -E %fedora).noarch.rpm

# 2. Enable the punktfunk COPR repo  ⚠️ requires the COPR to be published (see callout above)
sudo wget -O /etc/yum.repos.d/_copr_punktfunk.repo \
  https://copr.fedorainfracloud.org/coprs/enricobuehler/punktfunk/repo/fedora-$(rpm -E %fedora)/

# 3. Layer punktfunk and reboot to activate the new deployment.
rpm-ostree install punktfunk
systemctl reboot
```

> The **reboot is mandatory** — `rpm-ostree install` stages a new deployment that only takes
> effect on the next boot. This is normal atomic-distro behavior, not a punktfunk quirk.

#### Updating a Path-C host — `rpm-ostree upgrade` is NOT enough

> ⚠️ **`rpm-ostree upgrade` will not update punktfunk on its own.** `upgrade` bumps the **base
> image** and only re-resolves *layered* packages **when the base changes**. A Bazzite base can
> sit frozen for months (a pinned `:stable` tag, a paused rebase), so `rpm-ostree upgrade` keeps
> reporting *"No updates available"* and your layered `punktfunk` stays put even after new RPMs
> land in the repo. (Diagnose: `rpm-ostree status` shows the base `Version:` unchanged, while
> `dnf -q repoquery --upgrades punktfunk` lists newer builds.)

To actually pull a newer host on a static base, force rpm-ostree to re-resolve just the punktfunk
layer — remove + re-add the same names in one transaction:

```sh
sudo rpm-ostree refresh-md --force
sudo rpm-ostree update \
  --uninstall punktfunk --uninstall punktfunk-web \
  --install   punktfunk --install   punktfunk-web
systemctl reboot
```

Or just run the helper, which detects what's layered and does the above. The `punktfunk` RPM
installs it, so on a layered box it's already there — no checkout needed (it's the same command the
web console's update hint prints):

```sh
sudo /usr/share/punktfunk/update-punktfunk.sh          # stage; reboot when ready
sudo /usr/share/punktfunk/update-punktfunk.sh --reboot # stage + reboot now
```

From a repo checkout (e.g. before the first install, or to run a newer helper than the layered RPM
carries) the same script runs directly — it only shells out to `rpm-ostree`/`rpm`/`systemctl`:

```sh
sudo bash packaging/bazzite/update-punktfunk.sh --reboot
```

> **Channel gotcha:** the re-resolve picks the highest version across **every enabled**
> `/etc/yum.repos.d/punktfunk*.repo`. If `punktfunk-canary.repo` is enabled alongside the stable
> `punktfunk.repo`, canary's `<next-minor>.0-0.ciN` **outranks** the stable `X.Y.Z-1` and the box
> silently tracks canary. Enable exactly one channel — set `enabled=0` in the other repo file.

## First-run setup and gotchas

The walkthrough — udev and group, `host.env`, the service, the firewall, verifying — is on the
[Bazzite docs page](https://docs.punktfunk.unom.io/docs/bazzite). Two things that bite here and
nowhere else:

- **Use `ujust add-user-to-input-group`, not `usermod -aG input`.** Bazzite is atomic; the plain
  usermod does not stick across an OS update.
- **rpm-ostree layering is the last resort**, not a fallback of equal standing: it slows every OS
  update and can block upgrades. The sysext overlays `/usr` at runtime, survives OS updates and
  needs no reboot, which is why it is Path A.
- **A dev unit can shadow the packaged binary.** `scripts/punktfunk-host.service` (the upstream/dev
  unit) assumes the binary at `%h/punktfunk/target/release/punktfunk-host`, while the packaged one
  is `/usr/bin/punktfunk-host`. If `systemctl --user cat punktfunk-host` shows an `ExecStart`
  pointing into a home directory, drop an override setting
  `ExecStart=/usr/bin/punktfunk-host serve`.

The user-facing gotchas are on the docs site, including the ds_inhibit SELinux storm with
DualSense-type pads — the `dontaudit`-vs-`allow` rationale for that one is the header of
`punktfunk-ds-inhibit.cil`.
