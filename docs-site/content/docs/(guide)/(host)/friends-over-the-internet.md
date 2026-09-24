---
title: Friends over the internet
description: Let a friend stream from your host without joining your home network — Porthole for PC friends, Tailscale sharing for everyone else.
---

Let a friend outside your home stream from your host, and reach nothing else on your network.
[Pairing](/docs/pairing) and [access levels](/docs/access-levels) already decide what their device
may do; this page only gets them to the host.

## Pick a path

| Your friend streams from | Use |
|---|---|
| A PC, Mac or Steam Deck with Steam | [Porthole](#porthole): nothing to set up on your router |
| A phone, tablet, Apple TV, or no Steam | [Tailscale machine sharing](#tailscale) |
| Anything, and you run your own router | [Port forwarding](#port-forwarding): not recommended, the only path that exposes the host to the internet |

## Pin the video port

Every path needs this first. Video normally uses a random UDP port per session; a tunnel or a rule
has to name one. Add this to `host.env` ([Configuration](/docs/configuration)) and restart the
host:

```ini
PUNKTFUNK_DATA_PORT=9779
```

Any free UDP port works except 9778, which browser streaming uses. The pinned port carries one
session at a time, so one friend at a time streams through it.

## Porthole

[Porthole](https://porthole.sestudio.org/) is a free Steam app that shares the ports you pick with
Steam friends, behind any router.

1. You both install Porthole from Steam.
2. You create a lobby and share UDP `9777` and `9779`, without remapping. Add TCP `47990` if the
   friend should browse your game library.
3. The friend joins with your share code or from the friends list, accepts the ports, and adds
   `127.0.0.1:9777` as a host in their Punktfunk client.
4. [Admit them as a guest](#admit-them-as-a-guest).

## Tailscale

Tailscale shares one machine with someone outside your tailnet. The default access rule still lets
them reach every port on it, so step 2 matters.

1. In the [admin console](https://login.tailscale.com/admin/machines), open the host's **⋯**
   menu → **Share** and send the invite. Any free Tailscale account can accept it. The friend
   installs Tailscale on the device they stream from (every client platform except LG webOS).
2. In [access controls](https://login.tailscale.com/admin/acls), replace the default `"src": ["*"]`
   rule (it includes shared users) with one rule for members and one for shared users, using the
   host's Tailscale IP:

   ```jsonc
   "acls": [
     { "action": "accept", "src": ["autogroup:member"], "dst": ["*:*"] },
     { "action": "accept", "src": ["autogroup:shared"], "dst": ["100.x.y.z:9777,9779"] }
   ]
   ```

   Add `47990` only if the friend should browse your game library.
3. The friend adds `100.x.y.z:9777` as a host, or opens a
   [link](/docs/presets-and-links#punktfunk-links) you send: `punktfunk://connect/100.x.y.z:9777`.
4. [Admit them as a guest](#admit-them-as-a-guest).

## Admit them as a guest

The friend's first connect waits in the console under **Waiting for approval**. Admit it with a
[PIN](/docs/pairing#pair-with-a-pin) read out over voice chat, not a bare **Approve**. Pick
**Controller only** and **Until they disconnect**, so nothing is left to clean up after the evening
([Temporary access](/docs/access-levels#temporary-access)). **Expire now** or **Unpair** ends a
running session at once.

## Port forwarding

Prefer [Porthole](#porthole) or [Tailscale](#tailscale): both keep the host unreachable from the
internet, and this doesn't.

Forward UDP `9777` and `9779` to the host. **Never forward `47990`, `47992` or `9778`**: the
management API, the web console and browser streaming.

Send your friend a [link](/docs/presets-and-links), not a bare address, so their first connect
checks the host's identity. Copy the **Deep link** from **Host** → **Connect a device** and swap
in your public address:

```
punktfunk://connect/<id>?host=203.0.113.5&fp=<64 hex>
```

Strangers will knock once the port is open. An unpaired device gets nothing. A knock from the
internet shows **From the internet** and has no **Approve**, because the name it shows is one it
chose itself. Click **Arm PIN** on that row: the PIN works only for that device, and its access
starts at Controller only until they disconnect. Read the PIN out over voice chat.

A pairing that keeps failing usually means a stranger is hitting the same rate limit. Wait a
moment and retry.

## Voice chat while they play

With Discord on the host, friends who stream in hear their own voices a beat late, because the
stream captures Discord's playback. In **Host** → **Settings** → **Audio**:

- **Where audio plays**: **Device and host**, so you hear the game too.
- **Voice chat**: **On the host**. Discord, Vesktop, WebCord, ArmCord, Legcord, TeamSpeak and
  Mumble then play on your speakers and stay out of the stream.
- **Voice chat apps**: add your browser (`firefox`) if you use Discord in a browser tab.

The same settings in `host.env` are `PUNKTFUNK_AUDIO_OUTPUT_MODE=host_and_client`,
`PUNKTFUNK_AUDIO_VOICE_CHAT=host` and `PUNKTFUNK_AUDIO_VOICE_APPS=firefox`. On Windows this needs
Steam installed: the host builds its silent audio output from Steam's streaming drivers. To keep
one friend from hearing the game at all, mute their session on the **Home** page.

## What not to use

ZeroTier, Hamachi and Radmin VPN work, but expose every port on the host to the friend.
playit.gg, ngrok and Cloudflare Tunnel can't carry a video stream.
