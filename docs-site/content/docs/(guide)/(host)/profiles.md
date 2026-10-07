---
title: Profiles
description: Give each person on the host a profile — their name on every session and, on Linux, a Steam of their own — and how players pick one.
---

Give each person who plays on this host a profile: their name on every session and their own play
stats, and on Linux a Steam account of their own in Big Picture. A host starts with one profile,
the owner, named after you; while it is the only one, nothing changes for anyone.

## Add a profile

1. In the [web console](/docs/web-console), open **Profiles** → **Add profile**.
2. Enter a **Name**, pick a **Colour**, and optionally a picture.
3. Under **Where this profile plays**, pick one:
   - **Shares Alex's desktop**, with your name: the same desktop and games, under another name.
   - **Own Steam** (Linux): Big Picture with a Steam account of its own.
   - **Own desktop** (Windows Server): [a Windows desktop of its own](#on-windows-server).

On Linux, **Own desktop** needs [**Reachable without logging in**](#on-linux) and stays greyed until
it is on. A host holds up to 8 profiles, and two can't share a name.

## Before the first Own Steam profile

An **Own Steam** profile plays in a gamescope of its own, so the TV's session stays untouched.
Install [gamescope](/docs/gamescope) first. Without it, the profile streams your desktop as the
owner would.

Adding the first one turns on **Steam per seat**
([what it does](/docs/configuration#what-some-settings-do)). If you set that off yourself, it stays
off, the profile uses your Steam, and its card says **Own Steam · needs Steam per seat**. Turn it on
under **Host** → **Show advanced**.

The first connect as that profile opens Steam's sign-in. Sign in once; the profile keeps it. Until
then the profile's card on the client says **Steam sign-in once**.

## On Windows Server

An **Own desktop** profile signs in to a Windows account of its own, so two people play at once
without sharing a desktop.

1. Open **Profiles** → **Seats**. Seats need Windows Server with the Remote Desktop Session Host
   role and licensing, and a GPU. The checks there list what is missing.
2. Tick **Seats are on**. Leave **Allow Remote Desktop from the network** off to limit Remote
   Desktop to this machine; tick it if you manage this server over Remote Desktop from another
   computer.
3. Open **Add profile** and pick **Own desktop**. Each profile gets a Windows account of its own,
   and the player signs in to Steam and the stores once per seat.

A player who picks a stopped seat sees `Getting Kid's desk ready…` while it starts. A card's
**Start**, **Stop** and **End session** do the same by hand; **End session** keeps the seat
running. **Remove** deletes the profile's Windows account and its files too.

### What a seat can do

A seat's desktop is one virtual screen at its player's resolution. It never shows, turns off or
powers down a monitor of this machine, and a second device on the same seat shares the screen.
Only the owner's desktop follows **Displays**; of a device's display settings, only **Largest
screen this device gets** and the scale follow it onto a seat.

Every seat shares one set of **Seats** settings on **Host**, shown while seats are on:

- **Seats kept warm**: the seats of the most recent players start with the host.
- **Stop an idle seat after**: 240 minutes by default; 0 never stops one.
- **End a seat's stream when its game exits**: off keeps streaming the seat's desktop.
- **Highest mode per seat**, such as `2560x1440@120`: a device's own cap outranks it.

**Library**, **Game sources** and **Plugins** are per seat: once a seat exists, the page's title
(**Anna's library ▾**) switches it to another seat's own.

## On Linux

**Reachable without logging in** makes the box answer clients from boot, with the lock screen still
on your monitor. A paired device that picks your profile starts a session of yours in the
background, and the monitor is never touched. It is also what gives a profile a desktop of its
own: **Own desktop** is a user of its own with its own Steam, and up to four seats share the box,
yours included.

1. Add your user to the group that may start the switch:

   ```sh
   sudo usermod -aG punktfunk-update $USER
   ```

2. Open **Profiles** and tick **Reachable without logging in**. Confirm with the console
   password. The page reloads while the host restarts, and the console asks you to sign in again.

What turning it on does:

- **Files move.** The host's identity, pairings, profiles, display and hook settings, and the
  console password are copied to `/var/lib/punktfunk`, owned by a `punktfunk` system user that
  runs the host from then on. Paired clients keep working: the fingerprint is the same.
- **You become a seat.** Your own `punktfunk-host` moves to the first seat's ports and serves
  your library, plugins and Steam from your own home. The console reaches it from **Library**,
  **Game sources** and **Plugins**; the page's title, **Anna's library ▾**, picks another seat's.
- **The monitor wins.** When you log in at the machine, the background session ends within a few
  seconds and your own session hosts you. Streams on the background session end, and the client
  reconnects. Nothing on your monitor is closed.
- **The console is the door's.** It listens on the same port with the same password.
- **Games share one folder.** Seats install Steam games into `/var/lib/punktfunk/games`, and you
  join its group, `punktfunk-games`, at your next login. To share a game you already have, open
  Steam's **Settings → Storage**, add that folder and move the game there. Each seat keeps its own
  Proton prefixes and shader cache.

Turning it off copies the files back to your home and your own host serves the box again. Seats of
their own stay on the box, stopped, and come back with the switch.

| Check | Fix |
|---|---|
| The switch says `Add … to the punktfunk-update group first` | Run the command in step 1, then try again. |
| A profile shows **Unavailable** | Its seat didn't start. **Doctor** names the failing check. |

## How players pick

On a host with two or more profiles, the Apple, Android, Linux and Windows apps ask **Who's playing
on Living Room PC?** on the first connect and remember the answer for that host. The host card then
shows the pick's initials, and **Switch profile…** in the host's menu changes it.

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
that profile with "Your profile was removed by the host's owner." For an **Own Steam** profile
named Kid, **Also delete Kid's Steam and saved games on this host** erases its Steam too; without
it, the files stay on the host. The owner can't be removed.
