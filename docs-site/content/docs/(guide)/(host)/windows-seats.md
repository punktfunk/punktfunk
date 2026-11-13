---
title: Seats on Windows
description: Let several people play on one Windows Server PC at the same time, each on a desktop of their own — what it needs, how to set it up from a fresh install, and what to do when a seat won't start.
---

A seat gives a [profile](/docs/profiles) a Windows desktop of its own, so two to four people can
play on one PC **at the same time**, each with their own games, Steam and saves. Playing at the
same time needs **Windows Server** ([Windows 11](#windows-11)). If the people sharing the PC take
turns, you want [take-turns profiles](/docs/profiles#on-windows) instead, on any edition of Windows.

## What you need

| | |
|---|---|
| Windows | **Windows Server 2025** or newer, installed with Desktop Experience. |
| Graphics card | One that encodes video, as for any host, with its driver installed. All seats share it. |
| Remote Desktop | The **Remote Desktop Session Host** role with a licensing mode. **Set up Remote Desktop** below installs both. |
| Licences | A Remote Desktop Services CAL for every player: [Microsoft's rules](https://learn.microsoft.com/windows-server/remote/remote-desktop-services/rds-client-access-license). Windows runs 120 days without a licence server. After that, seats sign in only with CALs from [one](https://learn.microsoft.com/windows-server/remote/remote-desktop-services/rds-activate-license-server). **Doctor** counts the days down. |
| Steam | Installed in its usual folder, `C:\Program Files (x86)\Steam`, and opened once. Seats get their sound from it, and their Steam games. |

Each seat is a Windows account of its own, `pf_seat1` to `pf_seat4`, made and managed by Punktfunk.
Nobody signs in to those accounts by hand.

A seat can sign in to an account this PC already has instead: one of its standard accounts, with
its own Steam and saves. Under **Add profile**, pick **Own desktop**, then **Existing account**,
the account and its Windows password. Sign the account out first. Punktfunk keeps the password
sealed on this PC.

- While it's a seat, nobody signs in to that account at the PC. The seat doesn't start while
  someone is signed in to the account another way.
- An administrator can't be a seat. Make a standard account for the player, or make the account a
  standard user in Windows.
- Its seat runs no [hooks](/docs/automation#hooks).
- If Windows wants the password changed, remove the profile, sign in to the account at the PC and
  set it, then add the profile again.

## Prepare Windows Server

A fresh Windows Server needs three things before the host goes on:

1. Install your graphics card's driver. Until then Windows lists **Microsoft Basic Display
   Adapter**, and seats find no graphics card.
2. Set the network to **Private**. A fresh Server calls its network Public, and then no other
   computer reaches the host or its console:
   [No client can reach a Windows host](/docs/troubleshooting#windows-firewall).
3. Install Steam, open it once and wait for its sign-in window. That first start installs the audio
   drivers seats play sound through. Nobody has to sign in.

## Set up seats

1. [Install the host](/docs/windows-host) and open the [web console](/docs/web-console).
2. Open **Profiles** → **Seats**. The list shows what this PC has and what is missing.
3. If Remote Desktop Session Host or its licensing is missing, press **Set up Remote Desktop**. It
   installs the role, which takes a few minutes. When the list says to restart the host, press
   **Restart host**, open **Profiles** → **Seats** again and press **Set up Remote Desktop** once
   more: it sets licensing to Per User and turns seats on.
4. Otherwise tick **Seats are on**. Leave **Allow Remote Desktop from the network** as you find
   it: it starts ticked on a server you already reach over Remote Desktop, and unticking it cuts
   that off.
5. Close **Seats**, press **Add profile**, and pick **Own desktop**. Repeat for each player.

Turning seats on installs the seat display driver, turns Remote Desktop on for this PC only and
records its certificate.

## Play on a seat

Players connect to **this PC**, the same host card as always, and pick their profile when the app
asks **Who's playing?** The host sends each player on to their seat.

The first connect to a stopped seat shows `Getting Kid's desktop ready…` for up to a minute while
Windows signs the seat in. [Moonlight](/docs/moonlight) and other apps that can't pick a profile
play on the owner's desktop. A seat carries the game's sound; a player's microphone doesn't reach
a seat.

On **Profiles**, a seat's card has **Start**, **Stop** and **End session**. A seat nobody plays on
stops after **Stop an idle seat after** under **Host** → **Seats**. The rest a seat shares with
Linux seats is under [What a seat can do](/docs/profiles#what-a-seat-can-do).

A seat's player opens their own Punktfunk from the seat's desktop with the tray's **Open
console**. On its **Plugins** page they install plugins from the Punktfunk store and turn them on.
Those run as the seat's account, each in its own sandbox, while the seat runs. On a seat on an
existing account only the player installs them.

## Steam on a seat

Steam runs once per Windows PC under its usual name, and a second start closes the first. So each
seat gets a copy of the PC's Steam of its own, and Punktfunk points every usual way of opening
Steam on a seat at that copy:

- the shared **Steam** icon on the desktop and in the Start menu,
- `steam://` links, such as a game's desktop shortcut,
- **Run Steam when my computer starts** in the seat's Steam,
- games started from Punktfunk's library.

While seats are on, the shared Steam icons start `punktfunk-tray.exe --steam`, which opens the
seat's copy on a seat and the PC's own Steam for everyone else. Turning seats off puts them back.
The copy is made the first time a seat starts; until it lands, Steam games don't start on that
seat.

Sign in to Steam once on each seat. One Steam account can play on one seat at a time, so each player
needs an account of their own; [Steam Families](https://store.steampowered.com/families) shares the
games. Seats install nothing twice: their Steam lists the PC's game folders first.

Don't pin `steam.exe` from `C:\Program Files (x86)\Steam` on a seat, or start it from File Explorer:
that starts the PC's Steam and closes the other one.

Two gaps remain ([#1873](https://git.unom.io/unom/punktfunk/issues/1873)). Starting Steam on the PC
closes a seat's Steam, so start the PC's first. And a Steam already open on a seat takes no
shortcut, link or library launch: open it from its tray icon and start the game there.

## Windows 11

Windows 11 runs one desktop at a time, so it can't play two people at once. It can still give each
person their own account, Steam and saves — they take turns. Under **Add profile** pick
**Own account (take turns)** and bind it to a Windows account; see
[take-turns profiles](/docs/profiles#on-windows). **Own desktop** (a seat, playing at the same time)
says **Needs Windows Server**, and turning seats on answers `Playing at the same time needs Windows
Server — this is a desktop edition of Windows. To take turns, add an Own account profile instead.`

Windows 11's [terms](https://www.microsoft.com/en-us/useterms) license it "for use by one person at
a time", so concurrent seats there break them. Punktfunk doesn't include, install or recommend a
Remote Desktop wrapper such as TermWrap. On a PC that already has one, seats turn on with this
warning and without support: `A Remote Desktop wrapper lets this desktop edition of Windows run
concurrent seats. Only Windows Server is supported for that. To take turns instead, add an Own
account profile.`

## Troubleshooting

**Profiles** shows the doctor's verdict under the cards. Press **Doctor** for every check, the PC's
first and then one group per seat, each with the next move. A seat's own log is
`C:\ProgramData\punktfunk\seats\hosts\<seat id>\host.log`, readable by Administrators.

### The console doesn't open from another computer

Windows marks the server's network Public, where the host's firewall rules don't apply.
[Set it to Private](/docs/troubleshooting#windows-firewall).

### No graphics card was found

The card's driver isn't installed, so Windows runs it as **Microsoft Basic Display Adapter**.
Install the driver from the card's maker.

### Remote Desktop Session Host isn't installed

Press **Set up Remote Desktop** under **Profiles** → **Seats**. Installing **Remote Desktop
Licensing** alone isn't enough: Session Host is its own part of the Remote Desktop Services role.

### The seat display driver isn't installed

A seat starting puts it back. If the line stays, turn seats off and on under **Profiles** →
**Seats**.

### Remote Desktop's certificate has changed

Windows renews the certificate Remote Desktop presents when it expires or the PC is renamed.
Seats only sign in to the one they recorded. Turn seats off and on under **Profiles** →
**Seats** to record the new one.

### The seat didn't start in time

The line says which step it stopped at. **Doctor** names the failing check; the seat's log has the
rest.

### A seat on an existing account won't start

The card's line names the next move. If the account's Windows password changed or expired, press
**Update password…** on the card and enter the new one. If the account is gone, or was deleted
and made again, remove the profile and add it again.

### Pairing isn't armed, or the library doesn't load

The app connected to a seat directly. Remove the seat's host card, connect to the PC itself and
pick the profile.

### Steam closes on another seat

That Steam was started as the PC's own, from `steam.exe` itself. Start Steam from the **Steam**
icon on the desktop or in the Start menu, or from Punktfunk's library.

### A game loses its controller when someone else switches apps

The game reads controllers through Microsoft GameInput, which gives them to the app that came to
the front last, on any seat or on the PC itself. **Players keep their controllers** under
**Host** → **Seats**, on by default, lets every game keep its controller in the background. A game
picks it up at its next start, so restart one that was already running.

### A controller plugged into the PC appears on a seat

Controllers plugged into the PC belong to the desk, and a seat's games and Steam don't list them.
Two cases Windows leaves open: a seat's Windows.Gaming.Input games still list an Xbox controller
plugged into the PC, without its input; and an Xbox controller plugged into the PC while a seat
already has its controller makes that seat's controller player 2 in XInput games until the seat's
next connection.

### A seat has no sound

Steam's audio drivers aren't on this PC: Steam is missing, or was installed and never opened.
Install Steam, open it once, then stop and start the seat.

### Seats stream without a picture

Windows' own Remote Desktop display driver is newer than Punktfunk's. Update Punktfunk.

### Remote Desktop's days without a licence server are over

Windows signs seats in for 120 days without a licence server. Set one up with a CAL for every
player: [Activate the Remote Desktop Services license server](https://learn.microsoft.com/windows-server/remote/remote-desktop-services/rds-activate-license-server).

## Turn seats off

Untick **Seats are on** under **Profiles** → **Seats**. Every seat stops and Remote Desktop goes
back to how it was. Uninstalling Punktfunk does the same. A seat account and its files go when you
remove its profile. A profile on an existing account gives that account back instead: the account,
its files and its Steam copy stay.
