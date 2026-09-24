---
title: Wake-on-LAN
description: Wake a sleeping host from a Punktfunk client — what has to happen first, the punktfunk wake command, and how to arm the host machine.
---

Punktfunk clients wake a sleeping host with a Wake-on-LAN magic packet. The clients do it out of
the box; the host machine has to be armed to wake, and that is where Wake-on-LAN usually fails:
see [Arming the machine](#arming-the-machine). To put the host to sleep, see
[Host power](/docs/host-power).

## Before you rely on it

- **The client has to see the host awake once.** It learns the host's network card address (MAC)
  from the host's local-network announcement and keeps it. Open the client on the same network
  while the host is on (`punktfunk discover` on the command line). Every client except the Linux
  app also takes a MAC typed in by hand.
- **Client and host share a network.** Magic packets don't cross subnets, a VPN or a mesh network.
- **Wired Ethernet is the sure thing.** Wi-Fi works when the adapter supports Wake on Wireless LAN
  (WoWLAN).

The packet isn't authenticated. A wrong address only makes the wake fail; the host's certificate
still guards the connection ([Security](/docs/security)).

## Waking from a client

**Auto-wake on connect** is on by default ([Client settings](/docs/client-settings#behavior)).
Opening a saved host that doesn't answer then:

1. Sends a magic packet and dials anyway: a host reached over a VPN never announces itself.
2. If the dial fails, shows **Waking…**, re-sends every 6 seconds and checks every second.
3. Connects when the host answers, for up to 90 seconds. The Apple and Android apps and Punktfunk
   Console then wait with **Try Again**; the Linux and Windows apps say the host didn't come
   online.

A host that wakes on a new address is updated in the saved record (Linux, Windows and Android
apps). Turn auto-wake off for hosts behind a VPN, which look asleep when they aren't.

A saved host's menu also has an explicit wake while it is offline and its MAC is known:

| Client | Explicit wake | Type a MAC by hand |
|---|---|---|
| Linux | **Wake host** sends the packet | not offered |
| Windows | **Wake host** sends the packet | **MAC (Wake-on-LAN)** under **Edit…** |
| macOS · iOS · iPadOS · tvOS | **Wake Host** waits on the **Waking…** screen | **MAC address** in **Edit Host** |
| Android · Android TV | **Wake host** waits on the **Waking…** screen | **Wake-on-LAN MAC** in **Edit host** |
| Punktfunk Console | **Wake host** in the host's options; the confirm button reads **Wake & Connect** | not offered |

- The Apple apps add a **Wake Host** action to Shortcuts; on iPhone and iPad say *"Wake ⟨host⟩
  with Punktfunk"*.
- Android 17 and later asks for the local-network permission before the app can reach anything on
  your network, wake packets included.
- The [Steam Deck plugin](/docs/steam-deck) wakes through the client, following its auto-wake
  setting.

### From the command line

`punktfunk`, the client command, ships with the Linux `punktfunk-client` packages and the Windows
client. In the Flatpak it is `flatpak run --command=punktfunk io.unom.Punktfunk`.

```bash
punktfunk wake <host-ref> [--wait]
```

`<host-ref>` is a saved host's id, name or address. Without `--wait` it sends the packet and
returns. With `--wait` it re-sends every 6 seconds and returns when the host answers, or after 90
seconds.

| Exit code | Meaning |
|---|---|
| `0` | Packet sent; with `--wait`, the host is up |
| `2` | With `--wait`, the host didn't answer within 90 seconds |
| `5` | No saved host matches, the name is ambiguous, or no MAC is known yet |
| `6` | That address isn't a saved host: pair it first |

`punktfunk launch` wakes the host by itself when auto-wake is on. See [Host CLI](/docs/host-cli)
for the other commands.

## Arming the machine

Punktfunk never changes these settings:

1. **BIOS/UEFI:** turn on **Wake on LAN**, **Wake on PCIe**, or your vendor's name for it.
2. **The network card:** arm it to wake on a magic packet, below.

### Check the host log first

A Linux host checks the card it announces and logs one line each time it starts announcing:

```text
Wake-on-LAN armed (magic packet) on host NIC
Wake-on-LAN is NOT armed on this host's NIC — clients cannot wake it from sleep.
Wake-on-WLAN armed (magic packet) on host Wi-Fi NIC
Wake-on-WLAN is NOT armed on this host's Wi-Fi NIC — clients cannot wake it from sleep.
```

The warning names the interface and the command that fixes it. No line means the host couldn't
tell (`ethtool` or `iw` missing, no answer from the driver) or local discovery is off. Read it
on the console's **Troubleshooting** page or with `journalctl --user -u punktfunk-host`. Windows
hosts don't run this check.

### Linux (wired)

`Wake-on: g` means armed for a magic packet; `d` means off.

```bash
ethtool enp5s0
sudo ethtool -s enp5s0 wol g
```

On many systems that resets at reboot. Check again after the next boot, and make it permanent in
your distribution's network configuration.

### Linux (Wi-Fi)

`ethtool` reports `Wake-on: d` for most Wi-Fi cards whatever their state. Ask `iw`, using the phy
behind the interface (`cat /sys/class/net/wlan0/phy80211/name`, usually `phy0`):

```bash
iw phy phy0 wowlan show
sudo iw phy phy0 wowlan enable magic-packet
```

Armed shows `* wake up on magic packet`. NetworkManager resets it on every connection, so on a
NetworkManager system set it on the connection instead:

```bash
sudo nmcli connection modify <connection> 802-11-wireless.wake-on-wlan magic
```

`command failed: Operation not supported` means the driver has no WoWLAN; that adapter can't wake
over Wi-Fi.

A wired host wakes and a Wi-Fi one never does? Turn off your access point's broadcast filtering
("multicast enhancement", IGMP snooping). Some laptops also cut the Wi-Fi card's power in deep
sleep, which no setting fixes.

### Windows

In **Device Manager** → **Network adapters**, open the adapter's properties. On **Power
Management**, allow the device to wake the computer. On **Advanced**, enable **Wake on Magic
Packet** (sometimes **Wake on Wireless LAN**) if it's listed. `powercfg /devicequery wake_armed`
lists every device allowed to wake the machine: if the adapter isn't there, nothing on the network
can wake this host.
