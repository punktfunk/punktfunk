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

On Linux, **Own desktop** stays greyed with **Needs the seats package**. A host holds up to 8
profiles, and two can't share a name.

## Before the first Own Steam profile

An **Own Steam** profile plays in a gamescope of its own, so the TV's session stays untouched.
Install [gamescope](/docs/gamescope) first. Without it, the profile streams your desktop as the
owner would.

Adding the first one turns on **Steam per seat**
([what it does](/docs/configuration#what-some-settings-do)). If you set that off yourself, it stays
off, the profile uses your Steam, and its card says **Own Steam · needs Steam per seat**. Turn it on
under **Host** → **Settings** → **Show advanced**.

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
running. [**Seats kept warm**](/docs/configuration#what-some-settings-do) starts the seats of
recently played profiles with the host, and a seat nobody plays on for 4 hours stops. **Remove**
deletes the profile's Windows account and its files too.

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
