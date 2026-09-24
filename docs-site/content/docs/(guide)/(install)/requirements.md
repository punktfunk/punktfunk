---
title: Requirements
description: What a Punktfunk host needs — system, desktop, GPU and network.
---

Check your host against this list before you install. What each combination can do, feature by
feature, is in the [Support matrix](/docs/support-matrix).

## The floor for a working host

| System | Floor |
|---|---|
| [Ubuntu](/docs/ubuntu) | 26.04 or newer |
| [Debian](/docs/debian) | 13 or newer, including LMDE 7 |
| [Fedora](/docs/fedora) | 43 or 44 |
| [Arch](/docs/arch), CachyOS | current |
| [Bazzite](/docs/bazzite), Fedora Atomic | current Bazzite; Fedora Atomic 43 or newer |
| [SteamOS](/docs/steamos-host) | SteamOS 3 |
| [NixOS](/docs/nixos) | 24.11 or newer, `x86_64-linux` |
| [Windows](/docs/windows-host) | Windows 11 22H2 (build 22621) or newer, x64. Windows 10 isn't supported |

On Linux the desktop sets the floor, not the package. **Ubuntu 24.04** installs `punktfunk-host`
but can't stream: its KWin 5.27 and GNOME 46 are below the [desktop floors](#desktop), and there is
no gamescope for it. **Debian 12** is below the package's glibc (2.39).

### Cinnamon, Linux Mint and LMDE

**Cinnamon can't stream its desktop.** Its compositor, Muffin, can't create a virtual display, and
no setting changes that. A Cinnamon box can still stream games through a headless gamescope, or
you log into a GNOME or Sway session instead.

| Edition | Base | Can it host? |
|---|---|---|
| **LMDE 7** | Debian 13 | ✅ Games through gamescope ([Debian](/docs/debian#cinnamon-linux-mint-and-lmde)) |
| **Linux Mint 22.x** | Ubuntu 24.04 | ❌ No gamescope, and its KWin and GNOME are below the floors |
| **Linux Mint 23** | Ubuntu 26.04 | ✅ Games through gamescope |

## Desktop

The host needs a **Wayland** session: a desktop you log into, a
[headless session](/docs/running-as-a-service), or none at all with gamescope, which the host
starts per client. On a box that boots to no session, set `PUNKTFUNK_COMPOSITOR=gamescope` in
`host.env`.

| Desktop | Floor |
|---|---|
| [KDE Plasma](/docs/kde) | Plasma 6. Above 60 Hz needs KWin 6.6; the headless session needs KWin 6.5.6 |
| [GNOME](/docs/gnome) | GNOME 48 |
| [Sway](/docs/sway), scroll | sway 1.8 |
| [Hyprland](/docs/hyprland) | Any version, with `xdg-desktop-portal-hyprland` installed |
| [Omarchy](/docs/omarchy) | Omarchy 4.0 |
| [gamescope](/docs/gamescope) | 3.16.22; 3.16.23 for the Steam overlay; HDR needs `punktfunk-gamescope` |
| Cinnamon | Can't host ([above](#cinnamon-linux-mint-and-lmde)) |

On Hyprland with `ecosystem.enforce_permissions` turned on, grant the host screencopy and virtual
input: a denial shows as black frames and dropped input, not as an error.

## GPU and driver

| GPU | Encoder | You need |
|---|---|---|
| NVIDIA | NVENC | Driver 535 or newer with its GL/EGL userspace, and `nvidia-drm modeset=1` |
| AMD, Intel | Vulkan Video (HEVC, AV1), VAAPI (H.264 and fallback) | Current Mesa with its Vulkan driver, and the VAAPI driver |
| None | Software H.264 | `PUNKTFUNK_ENCODER=software`, a fallback rather than a daily driver |

Your distro's install page installs the right driver packages. On Intel Gen12 (Tiger Lake) and
newer, including Arc, encoding needs the HuC firmware loaded: check `dmesg | grep -i huc` if encoding
fails. GeForce cards cap how many NVENC sessions run at once, which matters only when many devices
stream together.

HDR on Linux needs gamescope with `punktfunk-gamescope`, or GNOME 50 mirroring a real HDR monitor:
[HDR](/docs/hdr).

## Network

Put the host and client on the same network: your LAN, or a VPN that joins them. Wired or fast
Wi-Fi works best. Don't port-forward the host; to let someone in from outside, see
[Friends over the internet](/docs/friends-over-the-internet). Ports: [Ports & firewall](/docs/ports).

## A client

Something to stream *to*: [Install a Client](/docs/install-client).
