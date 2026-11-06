---
title: Someone's own account
description: Give a person a profile on their own Linux login, opened only with their password or their own device, and sign its devices out.
---

A profile can play on a person's own login of this Linux machine, with their home, Steam and
saves, and only their password or a device they signed in opens it.

## Add the profile

1. Turn on [**Reachable without logging in**](/docs/profiles#on-linux).
2. In the [web console](/docs/web-console), open **Profiles** → **Add profile**, enter a **Name**,
   and pick **Own desktop**, then **Existing account**.
3. Type the account's **User name** and **Their password**, then press **Add profile**.

Punktfunk checks the password once against the machine's login and keeps none. The profile then
plays in a background session beside whatever is on the monitor, with the box's games.

Any ordinary user of this machine can be one, a directory account such as `jdoe@corp.example.com`
included. Not you, the owner, who has a profile already, and not an account another profile uses.
On Windows Server, an existing account works differently: see
[Seats on Windows](/docs/windows-seats#what-you-need).

## Connect

When someone picks the profile, the Apple, Android, Linux and Windows apps ask for the
account's password, on a TV or with a controller too. The prompt has one switch, off by default:

| The device has | The switch | What it does |
|---|---|---|
| Face ID, Touch ID or Optic ID | **Unlock with Face ID on this device**, named after the device's own | From then on a face or finger match opens the desktop instead of the password. |
| A strong fingerprint or face unlock on Android | **Unlock with fingerprint on this device** | The same. |
| Windows Hello | **Unlock with Windows Hello on this device** | The same, with Hello's face, finger or PIN. |
| None of these | **Stay signed in on this device** | The device rejoins the running desktop without the password. |

With the switch off, the device stays signed in while it plays and for 10 minutes after it leaves,
at most a day. A match that fails, or a face or finger newly added to the device, brings the
password prompt back, where the switch sets the unlock up again.

A desktop that isn't running starts only from the password or a face or finger match. A device
that only stays signed in asks for the password then.

After 5 wrong passwords a device waits 15 minutes, and only 2 wrong passwords in 15 minutes reach
the account, so a device can't lock its person out at the desk.

## Who can open it

- **Opens it:** the account's password, typed in the app on a paired device, and the devices on
  its [signed-in list](#signed-in-devices). At the machine itself, the person logs in as always.
- **Can't:** the console password, the card's **Start**, **Wake**, a browser, a paired device
  without the password, and any other profile.

## Signed-in devices

The profile's card under **Profiles** lists each device signed in to it, as **Signed in for now**,
**Stays signed in** or **Unlocks with a fingerprint or face**, with when it was last used.

- **Sign out** takes one device off the list, a lost phone for one. It needs the password on its
  next connect, and its face or finger unlock is gone.
- **Sign out everywhere** does the same for every device.

## What runs there

- **They come first.** While that person is signed in at the machine, the profile doesn't start,
  and one that is running ends within seconds of their login. Their session is never touched.
- **The desktop keeps running.** It doesn't stop when idle, and **Seats kept warm** never starts
  it: only its person's unlock does. **Stop** on its card logs it off, and unsaved work there is
  lost.
- **The password opens the wallet.** When the typed password starts the desktop, KWallet opens
  with it on a machine whose own login opens KWallet. A desktop that a face or finger match
  started keeps the wallet locked until they type its password in the stream.
- **Nothing is set up in the account.** Punktfunk makes no user or group and doesn't prepare their
  home; the apps the profile runs write there what they always write. **Remove** ends only the
  profile's own session. The account and its files stay.
- **The console manages nothing inside.** No plugin runs there, and the console adds no plugin,
  folder access or emulator: **Plugins** and **Library** → **Sources** say so for that profile. It
  runs no [hooks](/docs/automation#hooks). A game from the box's library starts what the library
  says.
- **No password to update.** Nothing stores it, so the card has no **Update password…**. If the
  account's password expires, sign in as that person once.

## Troubleshooting

### The app says `This profile needs its account password. Update Punktfunk on this device.`

That app can't ask for a password. Update it, or connect from the Apple, Android phone, Linux or
Windows app.

### `Too many wrong passwords. Try again in 12 minutes.`

Too many wrong passwords reached this device or the account. Wait the time it names, then type the
password again.
