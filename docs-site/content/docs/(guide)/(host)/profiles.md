---
title: Profiles
description: Give each person on the host a profile — their name on every session and, with seats, a desktop of their own — and how players pick one.
---

Give each person who plays on this host a profile: their name on every session and their own play
stats, and with seats a desktop, a Steam and controllers of their own. A host starts with one
profile, the owner, named after you; while it is the only one, nothing changes for anyone.

## Add a profile

1. In the [web console](/docs/web-console), open **Profiles** → **Add profile**.
2. Enter a **Name**, pick a **Color**, and optionally a picture.
3. Under **Where this profile plays**, pick one:
   - **Shares Alex's desktop**, with your name: the same desktop, Steam and games, under another
     name.
   - **Own account (take turns)** (Windows): its own Windows account, Steam and saves, one person
     at a time — see [On Windows](#on-windows). Any edition of Windows.
   - **Own desktop**: a desktop of its own that plays at the same time as the others, with its own
     Steam and controllers, on [Windows Server](#on-windows-server) or [Linux](#on-linux). It
     gets a new account, or someone's own account under **Existing account**: one of this PC's
     on Windows Server, a login of this machine on Linux.

On Linux, **Own desktop** needs [**Reachable without logging in**](#on-linux) and stays greyed until
it is on. A host holds up to 8 profiles, and two can't share a name.

## On Windows

**Own account (take turns)** binds a profile to a Windows account on this PC. When someone picks it,
Punktfunk brings that account to the screen and streams it, so each person has their own desktop,
Steam and saves. One person streams at a time — picking another profile switches the PC to that
account, the way Windows' own **Switch user** does. This needs no Windows Server, no Remote Desktop
and no extra licence. To play at the same time instead, see [On Windows Server](#on-windows-server).

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
shares the games.

An account has to have been signed in since the PC last started before a profile can switch to it;
until then its card reads `Sign Kid in at the host once.`

## On Windows Server

An **Own desktop** profile signs in to a Windows account of its own, so two people play at once
without sharing a desktop. [Seats on Windows](/docs/windows-seats) has what it needs, the setup,
an existing account as a seat, and Steam on a seat.

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
   `services.punktfunk.seats.enable = true`, rebuild and restart: the users in
   `services.punktfunk.host.users` join the group.

2. Open **Profiles** and tick **Reachable without logging in**. Confirm with the console
   password. The page reloads while the host restarts, and the console asks you to sign in again.

What turning it on does:

- **Files move.** The host's identity, pairings, profiles and the console password are copied
  to `/var/lib/punktfunk`, owned by a `punktfunk` system user that runs the host from then on.
  Display settings and hooks stay yours. Paired clients keep working.
- **You become a seat.** Your own desktop is the first seat, with your library, plugins and Steam.
- **The monitor wins.** When you log in at the machine, the background session ends within a few
  seconds and your own session hosts you. Streams on the background session end, and the client
  reconnects. Nothing on your monitor is closed.
- **The console stays put.** It listens on the same port with the same password.
- **Seats see your games.** Every seat shows your Steam libraries: its Steam plays, updates and
  runs Proton from them while the files stay yours. A library on a filesystem other than btrfs,
  ext4, xfs or f2fs stays hidden from seats. New games a seat installs go to
  `/var/lib/punktfunk-seats/games`.
- **Your library is the box's.** Every profile's apps list the games your own host found. A
  profile with a desktop of its own plays your Steam games with its own Steam, and ROM Manager
  games with the box's emulators and its own saves. RPCS3, shadPS4, Supermodel, Vita3K and Xenia
  Canary play on your desktop only. So does an emulator installed while seats were off or by an
  earlier version, until you reinstall it under **Library** → **Sources** → **Emulators**. Your
  own custom emulators and games from other launchers play on your desktop only.
  Change the library on your own **Library** page; another seat's is read only.
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

### A seat on someone's own account

**Existing account** under **Own desktop** plays on a person's own login of this machine, and only
their password or a device they signed in opens it. [Someone's own account](/docs/own-account)
covers adding one, what the person sees when they connect, and signing devices out.

## What a seat can do

A player who picks a stopped seat sees `Getting Kid's desktop ready…` while it starts. A card's
**Start**, **Stop** and **End session** do the same by hand; **End session** keeps the seat
running.

A seat's desktop is one virtual screen at its player's resolution. It never shows, turns off or
powers down a monitor of this machine. A second device on the same seat shares the screen when
its **A second device connects** is **Shares the screen**, and is told the seat is busy
otherwise. Only the owner's desktop follows **Displays**; of a device's display settings, only
**Largest screen this device gets**, the scale and **A second device connects** follow it onto a
seat. With **Browser streaming** on, a browser plays on its profile's seat too, on the seat's own
[port](/docs/ports).

Every seat shares one set of **Seats** settings on **Host**, shown while seats are on:

- **Seats kept warm**: the seats of the most recent players start with the host.
- **Stop an idle seat after**: 240 minutes by default; 0 never stops one. Your own seat never
  stops for this. A seat on [someone's own account](/docs/own-account) neither stops for this nor
  starts warm.
- **End a seat's stream when its game exits**: off keeps streaming the seat's desktop. A profile
  can [pick its own](#change-or-remove-a-profile).
- **Highest mode per seat**, such as `2560x1440@120`: a device's own cap outranks it.
- **Seats update your games** (Linux): off, seats play the games you installed but can't update
  them or install into your libraries. A seat picks up a change when it next starts.

## Each profile's own library

Every profile plays the same games, and each can see them its own way. Once the host has a second
profile, the title of **Library** is a pick (**Anna's library ▾**) that lists every profile. The
pick stays as you move between pages. In a profile's library you can:

- **Hide for Anna**: the game leaves Anna's shelf and can't be started as Anna. **Show for Anna
  again** brings it back.
- Set a game's **Stream** settings for Anna alone. They show **Only for Anna**, and **Use Enrico's
  setting** drops them.
- Pick Anna's own cover or artwork for a game; **Use Enrico's picture** drops it.
- Under **Sources**, turn a source off to hide all its games from Anna.

A profile only ever narrows your library. A game you hide, or a source you turn off, in your own
library stays hidden for everyone, and shows there as **Turned off in Enrico's library**. Shared
games are added, edited, installed and removed in your library, which the link under the title
opens. None of this needs the profile's desktop to be running.

A profile with a desktop of its own on a **New account** can also have games nobody else has.
**Add game for Kid** adds a game or a command that is kept on Kid's desktop and runs there as
Kid. It shows **Only for Kid** and is on Kid's shelf whether the desktop is running or not. Adding,
changing or deleting one starts Kid's desktop first, which takes about 20 seconds, and a cover
picked from a file on that desktop shows only while it runs. A profile on
[someone's own account](/docs/own-account) has none: a game added from the console would run as
that person.

The Apple, Android, Linux and Windows apps show a player the library of the profile they play as.
Hiding a game is for tidiness, not a lock: a player who picks another profile sees that
profile's games, and a person on [their own account](/docs/own-account) can change their own
copy.

**Plugins** and **Automation** follow the same pick for a profile with a desktop of its own.

## How players pick

On a host with two or more profiles, the Apple, Android, Linux and Windows apps ask **Who's playing
on Living Room PC?** on the first connect and remember the answer for that host. The host card then
shows the pick's initials, and **Switch profile…** in the host's menu changes it.

The controller interface asks on every connect instead:

- Turn on **Remember on this device** under the profiles to stop the question on that host. The
  card then shows the pick's initials.
- The button under a host card, **Playing as Kid** or **Who's playing?**, changes the pick or sets
  **Always ask**.
- On **Games**, the pill with the host's name opens **Host and profile**: pick the host whose games
  show and who plays there.

Elsewhere:

- A [`punktfunk://` link's `as=`](/docs/presets-and-links) or the command line's
  [`--as`](/docs/clients#scripting-the-punktfunk-cli) plays as a profile for one connect.
- The [Decky plugin](/docs/steam-deck) shows one chip per profile under the host.
- [Moonlight](/docs/moonlight) and the other clients play as the owner.

Anyone on a paired device may pick any profile, except one you [lock](#lock-a-profile) and one on
[someone's own account](/docs/own-account), which asks for its password. What a device may do
stays its [access level](/docs/access-levels).

## Lock a profile

A locked profile opens only on the devices you choose. On the profile's card, **Lock** it, then
**Add a device** from the devices paired with this host, Punktfunk or
[Moonlight](/docs/moonlight). Unlocking, giving it a PIN and adding a device ask for the console
password.

- A device on its list opens it as before. Any other device doesn't see it when it picks a
  profile, and a connect that names it is told `This device can't open this profile. Ask the
  host's owner to add it.`
- A **PIN** of 4 to 8 digits lets a device off the list in. A locked profile with a PIN shows in
  every device's picker, and the right PIN puts that device on its list for the visit, or until
  you sign it out when the device stays signed in. A device with a fingerprint reader or face
  unlock can keep that instead, and opens the profile with it from then on. An app that can't
  ask for a PIN yet says `This profile needs its PIN. Update Punktfunk on this device.`
- Wrong PINs make a device wait, as a wrong account password does. Ten wrong PINs for one profile
  within 15 minutes, from any devices, stop PIN entry for it for a while; the devices on its list
  still open it.
- Signing a device out ends its session, and so does locking a profile it plays unless you keep
  that device when you lock.
- The devices on its list can also change this profile's own games and game-exit choice, once
  the Punktfunk apps offer it.
- Locking the owner profile also keeps Moonlight to the Moonlight devices on its list.
- A profile that shares your desktop opens with your lock, your devices and your PIN: one
  desktop, one lock. A profile on its own Windows account (take turns) has a lock of its own.
- **Unlock** keeps the PIN for the next lock. Locking again starts from the devices you choose
  then.

The locks live in `profile-locks.json` beside the host's pairings, readable by the host alone. A
file the host can't read keeps every profile locked, and the console says so: fix or remove it.
Removing it unlocks every profile.

## Where a profile shows

- **Now** on **Home** puts the player's picture and name before the device, once the host has
  two profiles.
- **Devices** → **Approve** names the profile a waiting device asks to play as.
- The [game library](/docs/game-library#play-stats) counts each profile's play time beside the
  totals.
- [Hooks and events](/docs/automation) carry the profile of the session.

## Change or remove a profile

On the profile's card: **Rename**, **Choose picture** or **Remove picture** (PNG or JPEG under
1 MB), and **Remove**. For a profile with a desktop of its own, **Rename** also offers **When the
game exits**: **As the host does** follows **End a seat's stream when its game exits**, and **End
the session** or **Keep streaming** set it for that profile alone. On
[someone's own account](/docs/own-account) a game's exit always goes back to their desktop. Removing asks for the console password and ends every session playing as
that profile with "Your profile was removed by the host's owner." For a profile named Kid whose
own Steam the host still keeps, **Also delete Kid's Steam and saved games on this host** erases
that Steam too; without it, the files stay on the host. The owner can't be removed.
