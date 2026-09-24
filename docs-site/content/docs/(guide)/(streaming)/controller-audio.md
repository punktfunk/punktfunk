---
title: Controller speaker and haptics
description: Feel a game's DualSense voice-coil haptics and hear the pad's speaker on the controller in your hands — what to turn on, and how to check it.
---

A wired DualSense in your hands plays what the game on the host sends to the controller's
voice-coil haptics and its built-in speaker. Nothing is sent while the pad is quiet.

## What you need

- **A DualSense or DualSense Edge plugged in over USB** on the client. Over Bluetooth the pad has
  no audio device and gets ordinary rumble.
- **A Linux, Windows or Android client.** **Controller haptics** is on by default; **Controller
  speaker** is on by default on Linux and Windows, off on Android
  ([client settings](/docs/client-settings#input)). On Android both also need **DualSense /
  DualShock passthrough (USB)** on. The Apple clients play neither.
- **Controller speaker** on for the host, in the web console under **Host → Settings → Audio**. It
  is on by default and covers the haptics too.
- **On a Linux host**, a game running under **GE-Proton 11-5 or newer**. Stock Proton doesn't
  route controller audio.
- **On a Windows host**, Steam installed: the pad's audio device uses Steam's streaming speaker
  driver.

## No Pro Audio switch on the host

The usual Linux advice for DualSense haptics is to set the pad's **Profile** to **Pro Audio**. You
don't need to for streaming: the controller the host gives the game already has the four-channel
output haptics need, the same outputs a DualSense has on SteamOS. In your sound settings it shows
as **Wireless Controller** with no profile selector.

A DualSense plugged into the host itself gets the same outputs from the ALSA profile the host
packages install (rpm, deb, Arch, Bazzite sysext). Replug the pad, or restart PipeWire, after
installing.

## Check it's working

The host log shows, per pad:

```text
pad-audio nodes minted (real-pad split: …)                  # Linux hosts
pad audio streaming (0xD1, Opus 48 kHz, silence-gated)
DS5 title asserted haptics-select (audio haptics) pad=0
```

The last line means the game found the controller's audio device and switched from rumble to
haptics. If you see it and feel nothing, [test the client](#test-the-client-without-a-host). If it
never appears, the game didn't find the device.

## If a game doesn't find it

On a Linux host, add GE-Proton launch options. Try this first; it opens the controller's audio
device directly:

```text
PROTON_DUALSENSE_HAPTICS_PREFER_NON_EVENT=1 %command%
```

Some titles also want:

```text
PROTON_SONY_WINDOWS_DEVICE_NAMES=1 PROTON_KEEP_SONY_AUDIO_ENDPOINT_VISIBLE=1 %command%
```

*Death Stranding Director's Cut* needs `PROTON_DUALSENSE_SPLIT_AUDIO=1 %command%`.

To see which device GE chose, add `WINEDEBUG=+pulse` and look for a line starting
`Routing DualSense`.

## On a Linux client [#on-a-linux-client-the-pads-own-profile-matters-too]

The voice coils are channels 3 and 4 of the pad's sound card, and most distributions present the
pad as stereo, or as a mono speaker plus headphones, which drops them. When the pad has no
four-channel output, the client switches its card to **Pro Audio** for the session and restores
your profile afterwards; it never saves the switch. SteamOS already has a four-channel output.

- To manage the card yourself, set `PUNKTFUNK_PAD_AUDIO_PROFILE=0` in the client's environment
  and pick a four-channel profile.
- A Flatpak client may not be allowed to switch the profile. If the log says so, set the
  controller's **Profile** to **Pro Audio** in your sound settings.
- The switch recreates the card's microphone too. If your [microphone](/docs/client-settings#audio)
  is the DualSense's own, that session falls back to your default one.
- A client killed mid-stream leaves the pad on Pro Audio until you replug it, log out or reboot.

### Test the client without a host

Plug in the DualSense and run:

```sh
punktfunk-session --pad-audio-test
```

It lists the DualSense outputs it finds, names the one it picked, and plays a three-second tone
into the voice coils. If the pad buzzes, the client works and the problem is on the host or in
the game. Add `--speaker` to test the speaker instead, `--seconds N` to run longer.

In a Flatpak install, including the Steam Deck:

```sh
flatpak run --command=punktfunk-session io.unom.Punktfunk --pad-audio-test
```

## Known limits

- **The pad's speaker stays silent.** The speaker shares channel 1 with the headphone jack. The
  client points it at the speaker when **Controller speaker** is on, but a game that sets the
  pad's audio itself overrides that. `PUNKTFUNK_PAD_SPEAKER_PATH` and `PUNKTFUNK_PAD_SPEAKER_VOLUME`
  ([client-side settings](/docs/configuration#client-side-native-clients)) try other values.
- **A real DualSense plugged into the host can take the audio.** Some titles find it first. Unplug
  it while you stream.
- **Titles that match the pad by container ID** don't recognise it on a Linux host. Titles that
  match by name or USB ids work.
- **A Windows host gives one pad controller audio by default.** `PUNKTFUNK_PAD_AUDIO_SLOTS` raises
  it to four ([Configuration](/docs/configuration)).
