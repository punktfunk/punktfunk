---
title: Security & Safe Use
description: What a streaming host actually exposes, why to keep it on a trusted network, and how Punktfunk protects you.
---

Read this before you put a host on a network you don't fully control. Punktfunk is built to be secure
**on a trusted local network**, and that's the setting we support today. This page is upfront about what
a streaming host is, what protects it, and where the honest limits are.

> **The short version**
> - **Keep the host on a network you trust** — your home LAN, or a private VPN that puts host and client
>   on the same subnet. **Do not port-forward it to the public internet.**
> - **A streaming host is remote control of the machine.** Anyone who can stream to it sees the screen
>   and can move the mouse, type, and act as a controller — the same as sitting at the keyboard.
> - **Pairing is the security boundary.** Require pairing (the default), pick a strong console
>   password, and review your paired devices from time to time.
> - **The Moonlight/GameStream plane is off by default on every install route.** It pairs over
>   plain HTTP, so turn it on only on a LAN you fully trust, and turn it back off when no
>   Moonlight client needs it.
>   [Details](#gamestream--moonlight-compatibility-is-the-weak-crypto-path).
> - **Be thoughtful about *which* machine you run it on** — especially on Windows, where the host runs
>   with high system privileges so it can do its job. Prefer a dedicated or gaming PC over one holding
>   your most sensitive data.

## What a streaming host really is

Low-latency desktop and game streaming means two things travel over the network: **the screen goes
out, and input comes back in.** A paired client doesn't just watch — it drives. Its mouse, keyboard,
and controller are injected into the host's desktop, so **for anything it can reach, a streaming client
is equivalent to a person sitting at that machine.**

That's the feature. It's also the risk to understand:

- The host can capture the **secure desktop** — UAC elevation prompts and the lock screen — so a
  connected client can see and interact with those too. (This is what lets you unlock and administer a
  headless box remotely; it's the same capability Sunshine and Apollo provide.)
- Injected input isn't sandboxed to a game. Whoever is streaming can alt-tab, open a terminal, read
  files, or change settings — whatever the logged-in session can do.

This is true of **every** remote-access and game-streaming tool, not just Punktfunk. The takeaway isn't
"don't use it" — it's "treat access to your host the way you'd treat handing someone your unlocked
keyboard." The rest of this page is about making sure only people you intend can get that access.

## Keep it on a trusted network

**Punktfunk assumes a trusted local network. It is not designed, tested, or hardened to be exposed to
the public internet — do not port-forward it.** There is no WAN-hardening story yet: no rate-limited
public authentication gateway, no DDoS protection, no assumption that hostile traffic is constantly
probing the ports. Exposing the streaming ports directly to the internet puts an interactive
control surface for your machine in front of the entire world.

If you want to stream from outside your home, tunnel in instead of opening up:

- **Use a VPN** — WireGuard, Tailscale, or your router's built-in VPN. This puts your remote client on
  the *same private subnet* as the host, so from Punktfunk's point of view it's still a local
  connection, and the tunnel (not Punktfunk) handles internet-facing authentication and encryption.
  Discovery, pairing, and streaming then work exactly as they do at home.
- **Don't** map a router port to the host. A port-forward turns "trusted LAN service" into
  "internet-facing service" with none of the protections that implies.

A note on **Windows network profiles**: the installer opens the streaming and console ports only on
**Private and Domain** networks — the profiles Windows uses for networks you've marked as trusted. On a
network Windows classifies as **Public** (cafés, hotels, conferences — the default for unknown
networks), the ports stay **closed**, so a laptop host won't accept connections there. That's the safe
default, and it's the behavior you want on the move. Two things follow from it:

- **If your home network is *misclassified* as Public, clients won't connect.** Set it to Private
  (Windows Settings → Network & internet → your network → **Private network**). The host also logs a
  warning at startup when it detects it's on a Public network, so this doesn't fail silently.
- **If you have a trusted network that Windows insists on marking Public** (some headless or
  no-gateway LAN setups), you can opt in during install — the **"Allow connections on Public
  networks"** checkbox (off by default). Only do this for a network you actually trust.

Either way, pairing is what ultimately gates access — but keeping the host off untrusted networks is
the first line, and on the move the safest habit is still to stop the service when you don't need it.

## What actually protects you

Punktfunk has **no accounts and no cloud**. Trust is established directly, device-to-device, and then
pinned. The layers, from the outside in:

- **Pairing is required by default.** A new device can't stream until it completes a one-time
  **PIN pairing ceremony** (SPAKE2): the host shows a 4-digit PIN, you enter it on the client, and the
  exchange cryptographically binds both identities. An attacker who doesn't know the PIN gets a
  *single online guess* — no offline cracking, no dictionary attack. See
  [Pairing & Trust](/docs/pairing).
- **Identities are pinned.** After pairing, the client remembers the host's certificate fingerprint and
  the host stores the client's. Reconnects are automatic and mutually authenticated; if a host's
  fingerprint ever changes, the client refuses to auto-trust it and forces re-pairing.
- **The host's admin API is loopback-only.** The management API's read-only status is reachable by
  paired clients over the LAN (authenticated by their certificate), but every state-changing action —
  arming pairing, removing devices, session control — is honored **only from the host machine
  itself**.
- **The web console is the admin surface, and it stays on your network.** It answers peers on the
  local network or a VPN (private and link-local addresses, Tailscale's `100.64.0.0/10`) and
  refuses the internet, because it performs those local-only actions on your behalf once you have
  logged in. Treat anyone who can reach port 47992 as a candidate administrator. To keep it to the
  host machine, set `PUNKTFUNK_UI_BIND=127.0.0.1` in `host.env`; the installers ask.
- **The web console has its own password**, on every platform — which makes it the real remote-admin
  credential. Linux packages and the SteamOS installer generate a random one into
  `~/.config/punktfunk/web-password`; on Windows the wizard lets you choose it (a strong random
  default is pre-filled) while a silent install generates one, stored readable only by
  Administrators and SYSTEM. After the first sign-in the console keeps only an argon2id hash, and
  the console process never holds the password or the management token in its environment. Pick a
  strong one; repeated wrong guesses are rate-limited per IP. To change or reset it, see
  [Forgot your Password?](/docs/forgot-password).
- **The [shared clipboard](/docs/clipboard) is opt-in at both ends.** The host never advertises it
  until an operator adds a line to `host.env`, and on your side it is a switch per *saved host*
  rather than a global one — because letting a machine read and write what you copy is a decision
  about that machine (Android is the one client where that switch starts on).

### Pairing policy: open hosts and trust-on-first-use

By default the host **requires** pairing: a device that hasn't completed the PIN ceremony (or been
approved from the console) can't stream. That's the right setting on any shared network.

`serve --open` turns it off. The host then advertises `pair=optional` and accepts unpaired clients,
and clients offer a **trust-on-first-use** (TOFU) path: the first connect shows the host's
fingerprint, you confirm it against the one the host logged at startup, and the client pins it. TOFU
cannot detect an impostor on that first connection — if someone is impersonating the host the very
first time, you pin the attacker. PIN pairing closes that gap (the SPAKE2 ceremony binds both
identities), which is why it's the default. Clients never offer TOFU to a host advertising
`pair=required`, a host typed in by hand, or one whose policy is unknown — those route straight to
the PIN. Once pinned, a fingerprint change forces re-pairing rather than re-trust.

(The developer/measurement host, `punktfunk-host punktfunk1-host`, has its own `--allow-tofu` /
`--pairing-pin` flags for test harnesses; nothing here applies to it.)

### GameStream / Moonlight compatibility is the weak-crypto path

To interoperate with stock [Moonlight](/docs/moonlight) clients, Punktfunk can also speak the legacy
GameStream protocol. **That plane does not get the protections above.** Its pairing runs over plain
HTTP, and its legacy control encryption can reuse GCM nonces — so an attacker already on your network
could sit in the middle of a pairing, or recover input from a session. It is safe only on a LAN you
fully trust. The native `punktfunk/1` plane is unaffected and always on; GameStream is a second
plane running beside it.

**It is off by default on every install route** — the Linux packages' unit runs the native-only
`serve`, the SteamOS installer, the NixOS module and the Windows installer all leave it off, and a
host you start yourself with `punktfunk-host serve` has it off. It comes on only when you ask:
`PUNKTFUNK_GAMESTREAM=1` in `host.env`, the installer's checkbox on Windows, `--gamestream` on SteamOS,
`services.punktfunk.host.gamestream = true` on NixOS, or `serve --gamestream` by hand — see
[Moonlight](/docs/moonlight). One exception: a Windows service you register by hand with a bare
`punktfunk-host service install` gets the built-in default, which *is* `serve --gamestream`. (Installs from before the opt-in change served Moonlight by default; an
upgrade switches them to native-only until you set the knob.)

The host doesn't hide it: whenever the compat planes come up, it logs a warning at startup naming
exactly this risk.

**To turn it off**, if a box already has it on (you set the knob, or an install from before the
opt-in change never dropped it):

- **Linux** — delete `PUNKTFUNK_GAMESTREAM=1` from `host.env` and drop any `ExecStart` drop-in that
  still passes `--gamestream`, then `systemctl --user restart punktfunk-host`. See [What the unit
  starts](/docs/running-as-a-service#what-the-unit-starts).
- **Windows** — from an elevated prompt, `punktfunk-host service install --gamestream=off`, then
  `punktfunk-host service restart`.
- **SteamOS** — re-run the Deck installer without `--gamestream`, and delete `PUNKTFUNK_GAMESTREAM=1`
  from `host.env` if you set it.
- **NixOS** — set `services.punktfunk.host.gamestream = false;` (this also drops the GameStream
  firewall ports).

If you do need Moonlight, keep that host on a network you trust and pair over it there — don't pair a
GameStream client on a network you share with strangers.

## Choosing which machine to host on

We've put real work into hardening the host — sealed capture and gamepad channels, user-mode drivers,
loopback-gated admin, pinned trust — and we'll keep at it. But security is also about *blast radius*:
if a host is ever compromised, or you misconfigure trust, what does the attacker get? So pick the
machine with that in mind.

### The Windows host runs with high privileges

To capture the secure desktop (UAC, lock screen) and stream across reboots with nobody logged in, the
Windows host installs a service that runs as **`LocalSystem` (SYSTEM)** — the highest local privilege on
Windows. This is the same design Sunshine and Apollo use, and it's what makes headless, log-in-optional
streaming possible. It also means the host is a high-value component: a compromise of the host, or a
device you paired that you shouldn't have, is a foothold at the most powerful level of that machine.

We mitigate this deliberately:

- **Punktfunk's own drivers are user-mode.** The virtual display, both virtual-gamepad drivers
  (DualSense / DualShock 4 / Edge / Deck, and Xbox 360 / XInput) and the virtual pointer are
  **user-mode (UMDF)** drivers, so a driver bug is contained to a restricted service account — never
  ring-0, never full-system. (This is why Punktfunk dropped ViGEmBus.) Audio is the one place a
  kernel-mode driver is unavoidable (Windows has no user-mode way to create an audio device), and
  Punktfunk deliberately ships none of its own: the "Punktfunk Speakers" and "Punktfunk
  Microphone" endpoints are instances of **Steam's vendor-signed streaming-audio drivers**,
  created on your box from your own Steam install. Older Punktfunk versions bundled VB-CABLE
  (a third-party kernel-mode driver from VB-Audio) for the microphone; if you have one, other
  applications may use it, so uninstalling Punktfunk leaves it in place — remove it through its
  own uninstaller.
- **Sealed internal channels.** The desktop-frame ring and the gamepad input/output channels are
  passed between the host and its drivers as duplicated handles to unnamed objects, so another local
  service can't open them by name to read your screen or forge controller input.
- **Secrets are locked down.** The management token, the host identity key, and the console password
  are stored with Administrators/SYSTEM-only permissions.

**The honest floor still applies.** None of this defends against an attacker who is *already* an
administrator or SYSTEM on the box — at that level they own the machine regardless of Punktfunk. And a
virtual display is a real monitor: any process already running in your desktop session can capture it
through the ordinary OS screen-capture APIs, exactly as it could capture a physical monitor. That floor
is the same for every virtual-display streaming stack.

One nuance specific to how the Windows host captures: because it reads the composed desktop image (what
the monitor shows) rather than going through Windows' screen-capture APIs, a window that hides itself
from *recording* tools with `WDA_EXCLUDEFROMCAPTURE` still appears in the stream — just as it appears to
anyone looking at the physical screen. Conversely, DRM-protected video (Netflix and the like) is blanked
by Windows for any capture path, so it shows as black rather than the protected frames. Neither weakens
Windows' protections: the first is exactly what a person at the screen already sees, and the second is
Windows enforcing its own rule. The consistent way to think about it is the one from the top of this
page — **a connected client sees and does what a person sitting at that machine could**, no more (and,
for DRM content, slightly less).

**Recommendation:** run the Windows host on a **dedicated or gaming PC**, not on a machine that also
holds your most sensitive material (work laptop, financial records, the box with your password vault).
A gaming rig you stream from is a great fit; your primary secrets machine is not.

### The Linux host runs as your desktop user

The Linux host runs inside your normal desktop session as your **regular user account**, not root — so a
worst-case compromise is scoped to that user rather than the whole system. The same network guidance
applies: keep it on a trusted LAN or a VPN, require pairing, and don't expose it to the internet.

## Plugins run code on your host

A [plugin](/docs/plugins) is third-party code you choose to install, and it runs alongside the host —
so installing one is a real trust decision, the same kind as installing any other program on that
machine. Punktfunk narrows it as far as it can:

- **Catalog entries are version- and hash-pinned.** Every plugin in the catalog names one exact
  version and that version's package hash, and the host re-checks the hash before it downloads
  anything. A package quietly republished under the same version number is refused, not installed.
- **The badges tell you who looked at it.** **Verified** means somebody at unom reviewed *that exact
  package* from the built-in catalog. **External source** (*from &lt;source&gt;*) means a catalog
  **you** added — still pinned and hash-checked, but curated by somebody else. **Unverified** means
  you installed it by hand from a package spec: nobody reviewed it, nothing pins it, and it stays
  marked that way for as long as it is installed.
- **Adding a catalog source is a one-time trust decision** — every plugin in that catalog becomes
  installable on this host. You can paste the source's `ed25519:` public key when you add it, and
  the host will then refuse any index from it that isn't correctly signed.
- **A plugin does not choose what runs.** Its package declares which programs the host may start
  for it, and with what shape of arguments; a library entry only fills in values, which the host
  checks and turns into one argument each. Paths outside what that package declares — or what you
  granted it with `plugins grant` — are refused. So a plugin publishing a tile cannot publish a
  command.
- **Each plugin runs in its own sandbox.** On Linux every plugin is a separate process in its own
  [bubblewrap](https://github.com/containers/bubblewrap) sandbox: an empty home, no network unless
  its manifest asked for one, and read access to exactly the paths it declared plus the ones you
  granted it. Your `~/.ssh`, your browser profile and the host's own credentials are not in it. Nor
  is the host's process — the sandbox has its own PID namespace, which is what makes this a
  boundary rather than a curtain: a plugin cannot reach the host through `/proc`, read its
  environment, or signal it. A box that cannot build a sandbox runs **no** plugins and says so on
  the Troubleshooting page.
- **Each plugin has its own token.** A plugin can register itself and reconcile its own library
  entries; using another plugin's id is refused. None of them is the full-admin `mgmt-token`, so no
  plugin can register hooks or admit devices.
- **On Windows the runner is a different account.** Its scheduled task runs as
  `NT AUTHORITY\LocalService`, **not** SYSTEM, granted read on exactly the token files and the TLS
  pin. Plugins there share that account, so a malicious one can still interfere with another
  plugin's files while both run — the per-plugin sandbox is Linux-only for now.

Install plugins only from sources you trust, and prefer Verified catalog entries. See
[Plugins](/docs/plugins) for the install flow and the CLI.

## A short hardening checklist

- **Require pairing** — it's the default; don't run `--open` / `--allow-tofu` except on a network you
  fully trust and control.
- **Use a strong console password** and keep it out of shared documents.
- **Restrict who can reach TCP 47992** at the host firewall if your LAN is shared — that port is the
  console, and the console is remote administration.
- **Stay on a trusted network** — LAN or VPN. Never port-forward to the internet.
- **Leave GameStream off** unless you specifically need Moonlight compatibility — it's opt-in on
  every route; if you turned it on for a trial, turn it back off. See
  [above](#gamestream--moonlight-compatibility-is-the-weak-crypto-path).
- **Only install plugins you trust** — prefer Verified catalog entries.
- **Review paired devices** in the web console periodically; remove anything you don't recognize.
- **Keep the host updated** — security fixes ship in new builds.
- **On portable hosts**, stop the service when you're on an untrusted network.

## Reporting a vulnerability

Found a security issue? **Email [security@punktfunk.com](mailto:security@punktfunk.com).** Please
don't open a public issue, pull request, or chat post for a suspected vulnerability — that exposes
other users before a fix is available.

Helpful things to include:

- The component and version — e.g. `punktfunk-host 0.22.3`, Windows or Linux, and which client.
- The impact, and the attacker's position (same LAN, a paired client, a local service account,
  admin, …).
- Steps to reproduce, a proof-of-concept, or a crash/log if you have one.

We acknowledge reports within **3 business days** and practice coordinated disclosure — we'll keep
you posted, agree a disclosure date, and credit you when the fix ships (unless you'd rather stay
anonymous). The full policy is in
[`SECURITY.md`](https://git.unom.io/unom/punktfunk/src/branch/main/SECURITY.md).
