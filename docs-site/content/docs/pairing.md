---
title: Pairing & Trust
description: Admit a new device once — approve it from the web console or type a PIN — and it reconnects on its own from then on.
---

Punktfunk has no accounts and no cloud. A new device is let in **once**, by you, on your network;
after that it reconnects automatically on a pinned identity, and the host lists it until you remove
it. There are two ways to let a device in.

## Approve it from the console (no PIN)

The fastest way: just **connect** from the new device. The attempt shows up in the
[web console](/docs/web-console) under **Pairing → Waiting for approval**, with the device's name
and fingerprint. Click **Approve** — optionally give it a label like "Living Room TV" — and it's
paired on the spot; its next connect goes straight through.

**Deny** only dismisses the request (it can knock again; it isn't a blocklist). Requests expire on
their own after 10 minutes.

## Pair with a PIN

When you're at the device and the console isn't handy, or for the very first device: in the console
open **Pairing** and click **Pair a device**. The host shows a **4-digit PIN** and counts down a
2-minute window — type the PIN into the client:

- **Native apps (Apple, Linux, Windows, Android):** select the host, or *Pair with PIN…* from its
  menu, and enter it.
- **Steam Deck (Decky plugin):** pick the host in the Quick Access panel — an unpaired one offers
  **Request access** (the console approval above) or **Use a PIN instead**.
- **Moonlight:** it runs the other way round — Moonlight shows a PIN, and you type it into the
  console's **Moonlight (GameStream) pairing** card, along with a name for the device. Arming
  doesn't apply. (Moonlight needs [GameStream compat on](/docs/moonlight) first.)

If the window lapses, arm it again. A `punktfunk://` link can't pair for you: one carrying an
address opens the client's trust prompt, pre-filled and with any fingerprint the link named, but
admitting the device is still this ceremony.

## Choosing access when you admit a device

Approving and deciding what the device may do are one dialog:

![Approve this device: name, access level, expiry, and the one-click Approve as guest](/img/console-approve-device.png)

The levels are **Full control**, **Controller only** and **View only** (**Advanced** opens the
individual toggles — [Access levels](/docs/access-levels)); expiry is **Never**, **Until they
disconnect**, or 1 h / 4 h / 8 h / custom. The defaults are right for your own new laptop;
**Approve as guest** is for a friend's device — Controller only, for 4 hours, then it expires on
its own. The same two controls sit on the **Pair a device** card, and apply to whichever device
completes the PIN.

**Until they disconnect** removes the device's record once its last session ends — after a minute's
grace, so a router blip or a client restart is not a re-pair. Nothing is left in the list
afterwards, which is the point: a grant that outlives the evening is a device that can come back
while nobody is watching. Coming back later is a fresh knock and a fresh PIN.

A device that knocked **from the internet** is listed as such and cannot be admitted with
Approve at all — the name it sent proves nothing there. Its row offers **Arm PIN** instead, which
names the **Pair a device** card below for that one device; fill in the card and the PIN it mints
works for nobody else. Read it out to whoever is holding the device. See
[Friends over the internet](/docs/friends-over-the-internet).

## Managing paired devices

The console lists every paired device with its access (and a live countdown for temporary grants).
From there you can change the level, extend or cut the expiry, or **remove** the device — removing
revokes it immediately, even mid-session. Re-pairing a removed device is just the PIN ceremony again.

**Naming a Moonlight device.** Every Moonlight-compatible client identifies itself with the same
built-in name, so several of them look identical in the list. Name it as you pair it — the field
sits beside the PIN — or later, with the pencil on its row ("Living room TV"). Either way the name
is stored on the host, so every browser sees it, and removing the device forgets it. Devices paired
with Punktfunk's own apps send a real name already and have no pencil.

Can't pair at all? [Troubleshooting → Pairing is rejected](/docs/troubleshooting#pairing-is-rejected--the-client-cant-connect).

## How it works, briefly

Each host has a stable identity (a certificate); clients pin its fingerprint, the host stores the
client's. The PIN ceremony is SPAKE2, so someone who doesn't know the PIN gets one online guess and
no offline attack. A host whose fingerprint changes is treated as an impostor — the client forces a
fresh PIN ceremony rather than re-trusting it. Pairing is **required by default**; the reduced-security
alternatives (`serve --open`, trust-on-first-use) exist for fully trusted single-user networks and
are covered in [Security & Safe Use](/docs/security#pairing-policy-open-hosts-and-trust-on-first-use).
Scripts can pair from a terminal — [the `punktfunk` CLI](/docs/clients#scripting-the-punktfunk-cli).
