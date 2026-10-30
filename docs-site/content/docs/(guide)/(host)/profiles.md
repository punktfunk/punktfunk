---
title: Profiles
description: Give each person on the host a profile — their name on every session and, with seats, a desktop of their own — and how players pick one.
---

Give each person who plays on this host a profile: their name on every session and their own play
stats, and with seats a desktop, a Steam and controllers of their own. A host starts with one
profile, the owner, named after you; while it is the only one, nothing changes for anyone.

## Add a profile

1. In the [web console](/docs/web-console), open **Profiles** → **Add profile**.
2. Enter a **Name**, pick a **Colour**, and optionally a picture.
3. Under **Where this profile plays**, pick one:
   - **Shares Alex's desktop**, with your name: the same desktop, Steam and games, under another
     name.
   - **Own account (take turns)** (Windows): its own Windows account, Steam and saves, one person
     at a time — see [On Windows](#on-windows). Any edition of Windows.
   - **Own desktop**: a desktop of its own that plays at the same time as the others, with its own
     Steam and controllers, on [Windows Server](#on-windows-server) or [Linux](#on-linux). On
     Windows Server it gets a new account, or one of this PC's accounts under **Existing account**.

On Linux, **Own desktop** needs [**Reachable without logging in**](#on-linux) and stays greyed until
it is on. A host holds up to 8 profiles, and two can't share a name.

## On Windows

**Own account (take turns)** binds a profile to a Windows account on this PC. When someone picks it,
punktfunk brings that account to the screen and streams it, so each person has their own desktop,
Steam and saves. One person streams at a time — picking another profile switches the PC to that
account, the way Windows' own **Switch user** does. This needs no Windows Server, no Remote Desktop
and no extra licence: it works on Windows 11 and every other edition, because nobody is playing at
the same time. To play at the same time instead, see [On Windows Server](#on-windows-server).

Set it up:

1. In Windows, make an account for each person (**Settings → Accounts → Other users**) and sign in
   to each one once, so Windows has it ready. You don't have to stay signed in.
2. In the console, **Add profile** → **Own account (take turns)**, and pick that person's Windows
   account. Repeat for each person.

A player who picks a profile whose account isn't on the screen sees `Switching to Kid…` while the PC
changes over; the stream lands once it does. A card's **Start**, **Stop** and **End session** do the
same by hand. The owner profile is **this desktop as it is**: picking it streams whoever is signed in
now and switches nobody, which is what you want for playing at the PC itself.

Each person needs their own Steam account; [Steam Families](https://store.steampowered.com/families)
shares the games. Because only one account is on the screen at a time, Steam opens cleanly for
whoever is playing — no second copy to collide with.

An account has to have been signed in since the PC last started before a profile can switch to it;
until then its card reads `Sign Kid in on the PC once`. Signing in from a cold start, with nobody at
the keyboard, is not built yet.

## On Windows Server

An **Own desktop** profile signs in to a Windows account of its own, so two people play at once
without sharing a desktop. [Seats on Windows](/docs/windows-seats) has what it needs, the setup
and Steam on a seat. In short: open **Profiles** → **Seats**, press **Set up Remote Desktop** if
the list asks for it, tick **Seats are on**, then **Add profile** → **Own desktop**. To sign the
seat in to a standard account this PC already has, pick **Existing account** and enter its
password; see [Seats on Windows](/docs/windows-seats#what-you-need).

A player who picks a stopped seat sees `Getting Kid's desk ready…` while it starts. With
**Browser streaming** on, a browser plays on a seat too, on the seat's own port, UDP 9783–9786.
A card's
**Start**, **Stop** and **End session** do the same by hand; **End session** keeps the seat
running. **Remove** deletes the Windows account punktfunk made and its files too. An existing
account stays, with its files.

### What a seat can do

A seat's desktop is one virtual screen at its player's resolution. It never shows, turns off or
powers down a monitor of this machine. A second device on the same seat shares the screen when
its **A second device connects** is **Shares the screen**, and is told the seat is busy
otherwise. Only the owner's desktop follows **Displays**; of a device's display settings, only
**Largest screen this device gets**, the scale and **A second device connects** follow it onto a
seat.

Every seat shares one set of **Seats** settings on **Host**, shown while seats are on:

- **Seats kept warm**: the seats of the most recent players start with the host.
- **Stop an idle seat after**: 240 minutes by default; 0 never stops one. Your own seat never
  stops for this: it runs your plugins.
- **End a seat's stream when its game exits**: off keeps streaming the seat's desktop.
- **Highest mode per seat**, such as `2560x1440@120`: a device's own cap outranks it.
- **Seats update your games** (Linux): off, seats play the games you installed but can't update
  them or install into your libraries. A seat picks up a change when it next starts.

**Library**, **Game sources** and **Plugins** are per seat: once a seat exists, the page's title
(**Anna's library ▾**) switches it to another seat's own.

## On Linux

**Reachable without logging in** makes the box answer clients from boot, with the lock screen still
on your monitor. A paired device that picks your profile starts a session of yours in the
background, and the monitor is never touched. It is also what gives a profile a desktop of its
own: **Own desktop** is a user of its own with its own Steam, and up to four seats share the box,
yours included.

1. Add your user to the group that may start the switch, then restart the computer. The host
   keeps the groups it started with, so logging out is not enough:

   ```sh
   sudo usermod -aG punktfunk-update $USER
   ```

   On SteamOS, run `bash ~/punktfunk/scripts/steamdeck/update.sh` in Konsole instead and enter
   your password: it sets the switch up and joins the group. Restart once after. On NixOS, set
   `services.punktfunk.seats.enable = true` and rebuild.

2. Open **Profiles** and tick **Reachable without logging in**. Confirm with the console
   password. The page reloads while the host restarts, and the console asks you to sign in again.

What turning it on does:

- **Files move.** The host's identity, pairings, profiles and the console password are copied
  to `/var/lib/punktfunk`, owned by a `punktfunk` system user that runs the host from then on.
  Display settings and hooks stay yours. Paired clients keep working: the fingerprint is the same.
- **You become a seat.** Your own `punktfunk-host` moves to the first seat's ports and serves
  your library, plugins and Steam from your own home. The console reaches it from **Library**,
  **Game sources** and **Plugins**; the page's title, **Anna's library ▾**, picks another seat's.
- **The monitor wins.** When you log in at the machine, the background session ends within a few
  seconds and your own session hosts you. Streams on the background session end, and the client
  reconnects. Nothing on your monitor is closed.
- **Browsers play on seats too.** With **Browser streaming** on, a browser lands on its profile's
  desktop as an app does: each seat streams to browsers on its own port, UDP 9783–9786. A seat
  that is already running follows the switch within a few seconds.
- **The console is the door's.** It listens on the same port with the same password. Host
  settings, Displays, Automation, Plugins and Health checks show your own host, which streams.
- **Seats see your games.** Every seat shows your Steam libraries: its Steam plays, updates and
  runs Proton from them while the files stay yours. A library on a filesystem other than btrfs,
  ext4, xfs or f2fs stays hidden from seats. New games a seat installs go to
  `/var/lib/punktfunk-seats/games`, and you join its group, `punktfunk-games`, at your next login. Each
  seat keeps its own Proton prefixes and shader cache.
- **Your library is the box's.** Every profile's apps list the games your own host found. A
  profile with a desktop of its own plays your Steam games with its own Steam, and ROM Manager
  games with the box's emulators and its own saves. RPCS3, shadPS4, Supermodel, Vita3K and Xenia
  Canary play on your desktop only. So does an emulator installed while seats were off or by an
  earlier version, until you reinstall it under **Library** → **Sources** → **Emulators**. Your
  own custom emulators and games from other launchers play on your desktop only.
  Change the library on your own **Library** page; another seat's is read only. Covers that Steam
  keeps in your home show while your host runs.
- **A seat has Game Mode too.** **Return to Gaming Mode** on a seat's desktop opens Steam's Game
  Mode on that seat, and Steam's **Switch to Desktop** goes back. The stream carries on, and
  neither touches the box's own session. The desktop's apps keep running underneath, and Steam
  keeps running when you disconnect in Game Mode. A seat starts in the mode it was last in.

Turning it off copies the files back to your home and your own host serves the box again. Seats of
their own stay on the box, stopped, and come back with the switch.

| Check | Fix |
|---|---|
| The switch says `Add … to the punktfunk-update group first` | Run the command in step 1, restart, then try again. |
| The switch is greyed out, with a line under it | The line names what this box is missing and the command that adds it. Reload the page after. |
| A profile shows **Unavailable** | Its seat didn't start. **Doctor** names the failing check. |

## How players pick

On a host with two or more profiles, the Apple, Android, Linux and Windows apps ask **Who's playing
on Living Room PC?** on the first connect and remember the answer for that host. The host card then
shows the pick's initials, and **Switch profile…** in the host's menu changes it.

The controller interface asks on every connect instead:

- Turn on **Remember on this device** under the profiles to stop the question on that host. The
  card then shows the pick's initials.
- The button under a host card, **Playing as Kid** or **Who's playing?**, changes the pick or sets
  **Ask every time**.
- On **Games**, the pill with the host's name opens **Host and profile**: pick the host whose games
  show and who plays there.

- A device that had its own Steam seat before keeps playing as the profile named after it, without
  being asked.
- A [`punktfunk://` link's `as=`](/docs/presets-and-links) or the command line's
  [`--as`](/docs/clients#scripting-the-punktfunk-cli) plays as a profile for one connect.
- The [Decky plugin](/docs/steam-deck) shows one chip per profile under the host.
- [Moonlight](/docs/moonlight) and the other clients play as the owner.

A profile is a name, not a lock: anyone on a paired device may pick any profile. What a device may
do stays its [access level](/docs/access-levels).

## Where a profile shows

- **Home** → **Sessions** puts the player's picture and name before the device, once the host has
  two profiles.
- **Devices** → **Approve** names the profile a waiting device asks to play as.
- The [game library](/docs/game-library#play-stats) counts each profile's play time beside the
  totals.
- [Hooks and events](/docs/automation) carry the profile of the session.

## Change or remove a profile

On the profile's card: **Rename**, **Choose picture** or **Remove picture** (PNG or JPEG under
1 MB), and **Remove**. Removing asks for the console password and ends every session playing as
that profile with "Your profile was removed by the host's owner." For a profile named Kid whose
own Steam the host still keeps, **Also delete Kid's Steam and saved games on this host** erases
that Steam too; without it, the files stay on the host. The owner can't be removed.
