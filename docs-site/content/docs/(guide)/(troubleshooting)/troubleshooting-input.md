---
title: Input & controllers
description: Fixes for stuck input, wrong characters, controllers games can't see, the virtual Steam Deck pad and the shared clipboard.
---

Fixes for keyboard, mouse, controller and clipboard problems.

## Keyboard and mouse

### My mouse and keyboard are stuck in the stream

The stream captures them on purpose. **Ctrl+Alt+Shift+Q** (**⌃⌥⇧Q** on a Mac) hands them back;
every client's shortcut is in [Getting your input back](/docs/shortcuts#getting-your-input-back).

### My keyboard types the wrong characters (`#` comes out as `\`)

The host session's keyboard layout doesn't match your keyboard. Punktfunk sends the key you press,
and the host's layout picks the character.

- KDE, GNOME: set the layout in the desktop's keyboard settings.
- sway, Hyprland and Game Mode: set it for the system, then reconnect.

  ```sh
  sudo localectl set-x11-keymap de pc105 nodeadkeys   # your layout, model, variant
  ```

  Game Mode on an older `punktfunk-gamescope` still types US, and the host log warns
  `this build ignores XKB_DEFAULT_*`. [Update](/docs/updating) it.

## Controllers

### A controller is detected but games don't see it (Linux)

The host can't open `/dev/uinput`, where it creates virtual pads. The console's
**Troubleshooting** page flags it under **Virtual controller support**. Join the `input` group, then
restart the computer:

```sh
sudo usermod -aG input "$USER"    # Bazzite: ujust add-user-to-input-group
```

### The pad works, but arrives as an Xbox 360 controller instead of a Steam Deck

The virtual Steam Deck pad attaches through `vhci_hcd`, and only members of the `punktfunk` group may
attach it. The console's **Troubleshooting** page names what's missing under **USB/IP
controllers**. Usually it's the group:

```sh
sudo usermod -aG punktfunk "$USER"    # then restart the computer
sudo modprobe vhci-hcd                # only if the check says the module isn't loaded
```

Join only on a machine you trust: the group can attach any emulated USB device. The same group lets
the host take over [Game Mode](/docs/troubleshooting-stream#game-mode-black-screen-on-connect-or-the-stream-is-stuck-at-the-boxs-resolution).

### Stream lags, then freezes, with a DualSense pad (Bazzite, SELinux)

On an SELinux host, `steamos-manager` floods SELinux denials each time the virtual DualSense opens,
and `setroubleshootd` turns the flood into a CPU storm that outlasts the pad by minutes.

```sh
sudo punktfunk-sysext reapply                 # Bazzite: installs the drop-in that silences it
sudo systemctl mask --now setroubleshootd     # any host; nothing depends on it
```

On a layered or bootc host, install the drop-in yourself:
`sudo semodule -i /usr/share/punktfunk/selinux/punktfunk-ds-inhibit.cil`. Setting the client's
**Controller type** to **Xbox 360** avoids it too, without adaptive triggers, lightbar and touchpad.

### A Steam Controller 2 is captured, but Steam's controller list stays empty

Only Steam reads this pad, through its `hidraw` node, so the pad does nothing until Steam can open
it.

- Windows host: run the Punktfunk installer again with **Controller drivers** on; it reinstalls
  the gamepad driver.
- Linux host: no `attached via usbip` in the host log means the pad never attached; fix
  [the Steam Deck pad](#the-pad-works-but-arrives-as-an-xbox-360-controller-instead-of-a-steam-deck)
  first. `attached via usbip` without a later `answering feature GET` means Steam can't open the
  node: install the packaged udev rules (`60-punktfunk.rules`), run
  `sudo udevadm control --reload && sudo udevadm trigger`, and reconnect.

The trackpads don't move the pointer while Steam is closed. That's normal while the pad is captured.

A 2015 Steam Controller takes the same steps on either host. A Windows host whose gamepad driver
is older than the host gives it an Xbox 360 pad until the driver is reinstalled. On Bluetooth it
needs Valve's Bluetooth firmware, which Steam installs over its cable; pair it in your device's
Bluetooth settings by holding Y while you press the Steam button.

### A controller plugged into the PC shows up on a seat (Windows)

The PC's host marks every controller plugged into it for the console session, so seats don't
list it. If a seat still does, the host isn't running on the PC: restart it from the console.
What Windows does past that mark is under
[Seats](/docs/windows-seats#a-controller-plugged-into-the-pc-appears-on-a-seat).

## Clipboard

### Copy and paste between host and client does nothing

Sharing needs two switches: **Host → Shared clipboard** on the host, and the per-host
toggle in your client.
[Why the toggle does nothing](/docs/clipboard#why-the-toggle-does-nothing-or-is-greyed-out) covers
both, and the setups where nothing crosses.
