---
title: Switching from Sunshine, Apollo or Vibeshine
description: Run Punktfunk next to your existing Sunshine-family host while you try it — one port to move — then what maps to what, and how to migrate for good.
---

Try Punktfunk next to Sunshine or one of its forks (Apollo, Vibeshine, …), then move over for
good.

## Can I keep Sunshine installed while I try it?

**Yes, with one port moved.** Both want **TCP 47990**: Sunshine for its web UI, Punktfunk for its
management API. Whichever starts first gets it, so a shared box works until one boot it doesn't.
Move Punktfunk's in `host.env` (`~/.config/punktfunk/host.env`, on Windows
`%ProgramData%\punktfunk\host.env`):

```sh
PUNKTFUNK_MGMT_BIND=0.0.0.0:47991
```

Then restart the host: `systemctl --user restart punktfunk-host`, or on Windows
`punktfunk-host service restart`. The guided installer does this for you when it finds Sunshine
running.

- Clients and the console find the new port on their own. A host you added to a client **by IP
  address** assumes the default port: add it again from the host list.
- On a Linux firewall, also allow the new port (`punktfunk-native` opens 47990):
  [Ports & firewall](/docs/ports). On Windows, `punktfunk-host service install` re-creates the
  host's firewall rule for the port in `host.env`.
- **Windows:** while streaming, Punktfunk turns your other displays off, and an Apollo virtual
  display counts as one. In the console, set **Displays** → **Your monitors while streaming** to
  **Stay on** ([Virtual displays](/docs/virtual-displays)).
- **Leave Moonlight compatibility off** while both are installed. GameStream uses the same fixed
  ports as Sunshine, so only one of them can serve Moonlight. Try Punktfunk with a
  [native client](/docs/install-client) meanwhile.

To see what is installed and who holds the port:

```sh
punktfunk-host detect-conflicts   # exits 1 only when another host runs or starts on its own
ss -lptn 'sport = :47990'         # Linux
netstat -ano | findstr :47990     # Windows
```

Running both is fine for a trial. If something acts up, stop the other host first.

## What maps to what

| In Sunshine / Apollo | In Punktfunk |
|---|---|
| Web UI on 47990 | [Web console](/docs/web-console) on 47992 |
| PIN pairing | PIN pairing, or **Approve** in the console with no PIN ([Pairing](/docs/pairing)) |
| Moonlight clients | Work once you turn on [GameStream](/docs/moonlight); the [native apps](/docs/install-client) get every feature |
| A virtual display driver (SudoVDA, …) | Built in: a display per client at its resolution and refresh ([Virtual displays](/docs/virtual-displays)) |
| `apps.json` | The [game library](/docs/game-library), filled by launcher [plugins](/docs/plugins) and custom titles from the console |
| Per-app prep/undo commands | [Per-app prep/undo](/docs/automation#per-app-prepundo), plus events and hooks |
| `sunshine.conf` | `host.env` ([Configuration](/docs/configuration)) and the console |
| HDR through the virtual display | [HDR](/docs/hdr): Windows, and Linux on gamescope or GNOME 50 |
| Clipboard, Wake-on-LAN | [Shared clipboard](/docs/clipboard), [Wake-on-LAN](/docs/wake-on-lan) |

## Migrating for good

1. **Install** Punktfunk ([Install the Host](/docs/install)) and move the port as above. Pair a
   native client and stream for a while.
2. **Bring your library over:** install the [plugin](/docs/plugins) for each launcher you had in
   `apps.json`, and add anything custom on the console's **Library** page.
3. **Remove the other host:** `sudo systemctl disable --now sunshine` on Linux, or
   `sc stop SunshineService` and its uninstaller on Windows. On Windows also remove its virtual
   display driver.
4. **Turn on Moonlight compatibility** if you still use Moonlight clients
   ([Moonlight](/docs/moonlight)). They pair again, with Punktfunk this time.
5. **Undo the port move** if you like: delete the `PUNKTFUNK_MGMT_BIND` line and restart the host.

Something not behaving?
[Troubleshooting → Another streaming host is installed](/docs/troubleshooting-connect#another-streaming-host-sunshine-apollo--is-installed).
