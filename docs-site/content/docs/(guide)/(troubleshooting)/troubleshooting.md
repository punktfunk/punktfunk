---
title: Troubleshooting
description: Find what you see and jump to its fix — install, connection, picture, input and audio problems.
---

Find what you see and follow it to the fix. Start on the web console's **Troubleshooting** page:
its **Health checks** catch most setup problems and give the command that fixes each.

## Install & startup

- [The Linux host service won't start](#the-linux-host-service-wont-start)
- [`nvidia-smi` says it can't communicate with the driver](#nvidia-smi-says-it-cant-communicate-with-the-driver)
- [`systemctl --user status punktfunk-web`: unit not found](/docs/troubleshooting-startup#systemctl---user-status-punktfunk-web-unit-not-found)
- [pacman: database already registered](/docs/troubleshooting-startup#pacman-error-could-not-register-punktfunk-database-database-already-registered)
- [The desktop won't start, or "GPU … not supported by EGL"](/docs/troubleshooting-startup#the-desktop-wont-start-or-gpu--not-supported-by-egl)
- [The session fails right after editing host.env](/docs/troubleshooting-startup#the-session-fails-right-after-editing-hostenv)
- [Windows: the host or the web console won't start](/docs/troubleshooting-startup#windows-the-host-or-the-web-console-wont-start)
- [Windows: the status icon is missing after an update](/docs/troubleshooting-startup#windows-the-status-icon-is-missing-after-an-update)

### The Linux host service won't start

Usually an old hand-copied unit in `~/.config/systemd/user/` shadows the packaged one, and
`systemctl --user status punktfunk-host` shows `status=203/EXEC`. Remove it:

```sh
rm ~/.config/systemd/user/punktfunk-host.service
systemctl --user daemon-reload
systemctl --user restart punktfunk-host
```

Any other failure: `journalctl --user -u punktfunk-host -e` names the reason.

### `nvidia-smi` says it can't communicate with the driver

The NVIDIA kernel module didn't load. With Secure Boot on (`mokutil --sb-state`), its signing key
isn't enrolled yet. Import the key, reboot, and on the blue **MOK Manager** screen, at the machine
itself, choose **Enroll MOK** → **Continue** → **Yes**, enter the password, then **Reboot**:

```sh
sudo mokutil --import /var/lib/shim-signed/mok/MOK.der    # Ubuntu
sudo mokutil --import /var/lib/dkms/mok.pub               # Debian (DKMS)
sudo akmods --force && sudo mokutil --import /etc/pki/akmods/certs/public_key.der   # Fedora
```

Or turn Secure Boot off in the firmware. Then `cat /sys/module/nvidia_drm/parameters/modeset` must
print `Y`. If it doesn't, turn modesetting on, rebuild the initramfs and reboot:

```sh
echo 'options nvidia-drm modeset=1' | sudo tee /etc/modprobe.d/nvidia-drm.conf
```

## Connection & discovery

- [Another streaming host (Sunshine, Apollo, …) is installed](/docs/troubleshooting-connect#another-streaming-host-sunshine-apollo--is-installed)
- [The host isn't found on the network](/docs/troubleshooting-connect#the-host-isnt-found-on-the-network)
- [No client can reach a Windows host](#windows-firewall)
- [The host is asleep and won't wake](/docs/troubleshooting-connect#the-host-is-asleep-and-wont-wake)
- [Pairing is rejected, or the client can't connect](/docs/troubleshooting-connect#pairing-is-rejected--the-client-cant-connect)
- [Video is slow to start, or fails across subnets](/docs/troubleshooting-connect#video-is-slow-to-start-or-fails-across-subnets)
- [A plugin's interface doesn't load](/docs/troubleshooting-connect#a-plugins-interface-doesnt-load)

### No client can reach a Windows host [#windows-firewall]

Setup opens the host's ports on **Private** and **Domain** networks only, so a network Windows marks
**Public** blocks every client. The host log warns about it at startup.

Set the network to Private in **Settings** → **Network & internet** → your network → **Network
profile type**. Running the setup again offers the same switch. On a trusted network that has to stay
Public, run this from an elevated PowerShell:

```powershell
punktfunk-host service install --allow-public-network=on
```

It opens the streaming ports on Public networks; the console's ports keep their scope.

## Picture & session

- [Black screen, but the client connects](/docs/troubleshooting-stream#black-screen--no-picture-but-the-client-connects)
- [Black screen with sound (Windows)](/docs/troubleshooting-stream#black-screen-with-sound-windows)
- [Capture fails: "Session creation inhibited" (GNOME)](/docs/troubleshooting-stream#capture-fails-session-creation-inhibited-gnome)
- [Games open on a physical monitor, not on the stream (Hyprland, sway)](/docs/troubleshooting-stream#games-from-my-library-open-on-a-physical-monitor-not-on-the-stream-hyprland--sway)
- [Game Mode: black screen, or stuck at the box's resolution](/docs/troubleshooting-stream#game-mode-black-screen-on-connect-or-the-stream-is-stuck-at-the-boxs-resolution)
- [The picture freezes for a moment, over and over (Windows)](/docs/troubleshooting-stream#the-picture-freezes-for-a-moment-over-and-over-windows)
- [Stutter, drops, or high latency](/docs/troubleshooting-stream#stutter-drops-or-high-latency)
- [The stream doesn't use the codec or HDR I picked](/docs/troubleshooting-stream#the-stream-doesnt-use-the-codec-or-hdr-i-picked)

## Input & controllers

- [My mouse and keyboard are stuck in the stream](/docs/troubleshooting-input#my-mouse-and-keyboard-are-stuck-in-the-stream)
- [My keyboard types the wrong characters](/docs/troubleshooting-input#my-keyboard-types-the-wrong-characters--comes-out-as-)
- [A controller is detected but games don't see it (Linux)](/docs/troubleshooting-input#a-controller-is-detected-but-games-dont-see-it-linux)
- [The pad arrives as an Xbox 360 controller instead of a Steam Deck](/docs/troubleshooting-input#the-pad-works-but-arrives-as-an-xbox-360-controller-instead-of-a-steam-deck)
- [Stream lags, then freezes, with a DualSense pad (Bazzite, SELinux)](/docs/troubleshooting-input#stream-lags-then-freezes-with-a-dualsense-pad-bazzite-selinux)
- [A Steam Controller 2 is captured, but Steam doesn't list it](/docs/troubleshooting-input#a-steam-controller-2-is-captured-but-steams-controller-list-stays-empty)
- [Copy and paste between host and client does nothing](/docs/troubleshooting-input#copy-and-paste-between-host-and-client-does-nothing)

## Audio

- [Audio stutters, and only the audio (Linux)](/docs/troubleshooting-audio#audio-stutters-and-only-the-audio-linux)
- [Streamed audio sounds worse than on the host (Windows)](/docs/troubleshooting-audio#streamed-audio-sounds-worse-than-the-host-does)
- [Audio lags behind the picture](/docs/troubleshooting-audio#audio-lags-behind-the-picture)
- [I hear myself](/docs/echo)

## Still stuck?

Collect the logs in one file and file a report: [Reporting an Issue](/docs/report-an-issue). The
console's **Troubleshooting** page shows the host's recent log at debug detail, whatever the log
level, and a **Sources** filter for [plugin](/docs/plugins) output.
