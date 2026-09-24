---
title: Connect with Moonlight
description: Stream from a Punktfunk host using any Moonlight client.
---

Turn on GameStream on the host and any [Moonlight](https://moonlight-stream.org/) client can stream
from it: a browser, a smart TV, a console, an old phone. Where a [Punktfunk app](/docs/clients)
exists, use it instead: it has lower latency and more features.

## 1. Turn on GameStream

GameStream is off by default. In the [web console](/docs/web-console), open **Host → Settings**,
turn on **GameStream**, then click **Restart Punktfunk**. Or:

| Where | How |
|---|---|
| Windows installer | Turn on **Moonlight compat** |
| Guided installer (Linux, SteamOS) | Add `--gamestream` |
| NixOS | `services.punktfunk.host.gamestream = true;` (also opens the firewall) |
| `serve` by hand | `punktfunk-host serve --gamestream` |
| `host.env` | `PUNKTFUNK_GAMESTREAM=1`, then restart the host |

GameStream pairs over plain HTTP, so use it on a network you trust. Video, audio and input are
encrypted.

## 2. Add the host in Moonlight

Open Moonlight. The host usually appears on its own; if not, use **Add Host manually** and enter
the host's IP address.

### Moonlight doesn't find the host

- **The ports are closed.** Open the GameStream ports listed on [Ports](/docs/ports). On Linux the
  packages ship a firewall rule to turn on:

  ```sh
  sudo ufw allow punktfunk-gamestream                               # ufw
  sudo firewall-cmd --permanent --add-service=punktfunk-gamestream  # firewalld
  sudo firewall-cmd --reload
  ```

  The Windows installer opens them on Private and Domain networks; **Public-network firewall
  rules** adds Public ones.
- **Sunshine, Apollo or a fork runs on the same machine.** They use the same ports. Run
  `punktfunk-host detect-conflicts` to list them, and stop them.

More in [Troubleshooting](/docs/troubleshooting-connect#another-streaming-host-sunshine-apollo--is-installed).

## 3. Pair

1. In the web console, open **Devices**.
2. In Moonlight, select the host. It shows a 4-digit PIN.
3. In the console's **Moonlight (GameStream) pairing** card, type the PIN, name the device, enter
   your console password and click **Submit PIN**.

The device appears under **Paired devices**. See [Pairing & Trust](/docs/pairing).

## 4. Stream

Moonlight lists **Desktop** and the games in the host's [library](/docs/game-library). The host
creates a display at the resolution and frame rate set in Moonlight's settings. **Resume** returns
to a session you left; the console's **Stop** ends it.

An `apps.json` in the host's config directory replaces that list, **Desktop** included.

## Tips

- **Codec:** HEVC is a good default; AV1 works if your client decodes it.
- **HDR:** Moonlight offers HDR only when the host can [capture and encode it](/docs/hdr#the-chain).
  Pick HEVC or AV1; H.264 stays SDR.
- **Bitrate:** the number covers video and error correction together. Start moderate and raise it.
- The host adapts error correction and bitrate to the loss Moonlight reports. Nothing to set.
- Moonlight's overlay and the Punktfunk [stats overlay](/docs/stats) measure different things.
- The console's mute, access and player-slot controls work on Punktfunk-app sessions only.
