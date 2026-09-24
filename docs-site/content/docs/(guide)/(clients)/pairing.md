---
title: Pairing & Trust
description: Admit a new device once — approve it from the web console or type a PIN — and it reconnects on its own from then on.
---

Let a new device in once, from the host's [web console](/docs/web-console), and it reconnects on
its own after that. There are no accounts and nothing leaves your network.

## Approve it from the console (no PIN)

1. On the new device, pick the host and choose **Request access**.
2. In the console, open **Devices**. The device is listed under **Waiting for approval**.
3. Click **Approve**, set its [access](#choosing-access-when-you-admit-a-device), enter your
   **Console password** and click **Approve**. The device connects straight away.

**Deny** only dismisses the request; the device can ask again. Requests expire after 10 minutes.

## Pair with a PIN

1. In the console, open **Devices**, click **Pair a device** and enter your console password. It
   shows a 4-digit PIN for two minutes.
2. On the device, pick the host, choose **Pair with PIN…** or **Use a PIN instead…**, and type
   the PIN.

If the PIN runs out, click **Pair a device** again. A `punktfunk://` link can't pair a device.

**Moonlight** works the other way round: Moonlight shows the PIN and you type it into the console.
See [Connect with Moonlight → Pair](/docs/moonlight#3-pair).

## Choosing access when you admit a device

![Approve this device: name, access level, expiry, and the one-click Approve as guest](/img/console-approve-device.png)

- **Access level**: **Full control**, **Controller only** or **View only**. **Advanced** opens the
  individual permissions ([Access levels](/docs/access-levels)).
- **Access expires**: **Never**, **Until they disconnect**, 1, 4 or 8 hours, or **Custom…**.
- **Approve as guest**: Controller only, for 4 hours. Use it for a friend's device.

The **Pair a device** card has the same two controls; they apply to whichever device uses the PIN.

**Until they disconnect** removes the device a minute after its last session ends. To come back, it
pairs again.

A device that asked **From the internet** can't be approved: its name proves nothing. Click **Arm
PIN** on its row to make a PIN only that device can use, and pass the PIN on. See
[Friends over the internet](/docs/friends-over-the-internet).

## Managing paired devices

**Devices → Paired devices** lists every device with its access and any expiry countdown. From
there you can change a device's access or expiry, or **Unpair** it, which cuts it off at once, even
mid-stream.

Moonlight clients all report the same name. Name one in the PIN card as you pair it, or later with
the pencil on its row.

Can't pair at all? See [Troubleshooting → Pairing is rejected](/docs/troubleshooting-connect#pairing-is-rejected--the-client-cant-connect).

## How it works

The host has a stable certificate; each side pins the other's fingerprint. The PIN exchange is
SPAKE2: someone without the PIN gets one online guess and nothing to attack offline. If a host's
fingerprint changes, the client refuses it and asks you to pair again. To run without pairing on a
fully trusted network, see [Security → Pairing policy](/docs/security#pairing-policy-open-hosts-and-trust-on-first-use).
To pair from a script, use [`punktfunk pair`](/docs/clients#scripting-the-punktfunk-cli).
