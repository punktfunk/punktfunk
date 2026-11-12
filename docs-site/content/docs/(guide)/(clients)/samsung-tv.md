---
title: Samsung TV
description: Stream to a Samsung TV with the Punktfunk app for Tizen — turn on browser streaming on the host, put the set in Developer Mode, install the app, pair and play.
---

The Samsung TV app is the [browser client](/docs/browser-client) packaged for Tizen: the same
page, started in Punktfunk Console for the remote. It lives in its own repository,
[client-tizen](https://github.com/punktfunk/client-tizen). It runs on sets from 2024 on (Tizen
8.0). It is a preview, and it is sideloaded: Samsung's store does not carry it.

The app is signed for the one set it is installed on, so there is no package to download and
open. Apps2Samsung does the signing and the install from your PC; the CLI route below does the
same by hand.

## 1. Turn on browser streaming on the host

1. In the host's [web console](/docs/web-console), open **Host** and turn on **Browser
   streaming**.
2. Leave **Browser origins** as it is. The list is a rule for browser pages; the TV app is not one
   and is admitted either way. Pairing is what lets it stream.
3. Click **Restart Punktfunk**.
4. Open TCP 47990 and UDP 9778 on the host. On Linux both are in the `punktfunk-native` firewall
   profile; on Windows the host opens them ([Ports](/docs/ports)).

## 2. Put the TV in Developer Mode

1. Open the **Apps** page from the TV's left menu (not Search).
2. Enter `12345` on a number pad. The remote has no digits: use the **123** or colour button, the
   SmartThings app's remote, or a USB keyboard.
3. Turn **Developer mode** on and enter your PC's IP address as the **Host PC IP**. Press **OK**.
4. Hold the power button until the Samsung logo shows. The set restarts with Developer Mode on.

`curl http://<tv address>:8001/api/v2/` shows `developerMode` and `developerIP` without touching
the set.

## 3. Install the app

**With Apps2Samsung (recommended).** [Apps2Samsung](https://github.com/Apps2Samsung/Apps2Samsung)
is a desktop app for Windows, macOS, Linux and Android that signs in to your Samsung account once,
makes the certificate for your set and installs apps on it.

1. Install Apps2Samsung and sign in to your Samsung account when it asks.
2. Pick **punktfunk** from its list and your TV as the target, then install.

**By hand, with Samsung's tools.** For developers, or a set Apps2Samsung does not reach.

1. Download `punktfunk-tizen-<version>.wgt` from the latest
   [client-tizen release](https://github.com/punktfunk/client-tizen/releases/latest). It is
   unsigned.
2. Clone [client-tizen](https://github.com/punktfunk/client-tizen) and build its Tizen CLI image:
   `docker build --platform linux/amd64 -t punktfunk-tizen-cli:10.0 tools/toolchain`.
3. Make a Samsung distributor certificate for your set: `sdb connect <tv address>:26101`, read the
   DUID with `sdb shell 0 getduid`, and run `samsung-tv-cert --duid <DUID> --profile punktfunk`
   (a browser login to your Samsung account is the one manual step).
4. Sign and install: `tools/sign.sh punktfunk-tizen-<version>.wgt`, then
   `TV=<tv address> tools/sign.sh install`.

## 4. Pair and stream

1. Open **punktfunk** from the Apps page. It starts in Punktfunk Console.
2. **Add a host** and type the host's address with the remote (the on-screen keyboard opens).
3. In the host's console, open **Devices** and click **Pair a device**, then type the PIN it shows
   on the TV ([Pair with a PIN](/docs/pairing#pair-with-a-pin)).
4. Pick a title, or stream the desktop.

In a stream, **Back** on the remote opens the quick menu: **Disconnect, keep the game running** is
the first row, **End stream** closes the title. The TV's **Home** leaves the stream the same way; the
title keeps running on the host and the library offers **Resume**. Back at the console's home asks
to exit the app.

A controller paired with the set works as on any client. The TV stays paired until you remove the
app; a reinstall keeps the pairing too.

## What the TV app does and does not do

- Video: H.264 and HEVC at the set's own resolution, up to 1080p60. No AV1, no HDR, no 4K.
- The set's game mode is on while the app runs.
- No host discovery: type the address. No Wake-on-LAN from the TV.

## Troubleshooting

### "Browser streaming is off on this host"

Step 1. The host answers the TV's first request only while its browser plane runs.

### "Nothing responded"

The address is wrong, the host is off, or TCP 47990 is blocked between the TV and the host. The
TV and the host need not share a subnet; they need a route and the two ports.

### The install fails with `Invalid certificate chain`

The set refuses a certificate not made for it. Apps2Samsung makes one for the set it installs
on; by hand, check the DUID in your distributor certificate is the one `sdb shell 0 getduid`
prints — it is not the one `webapis.productinfo.getDuid()` returns.

### The app is gone after a firmware update

Sideloaded apps can vanish after a major firmware update. Install it again; the pairing is kept.

### The Frame

A set in the Frame line on Tizen 9 has refused certificates even when they were made for it. Not
solved yet.
