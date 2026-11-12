---
title: "Roadmap"
description: "Where Punktfunk is heading: the live roadmap on improve unom, and what is not planned."
---

The roadmap is on [improve unom](https://improve.unom.io/p/punktfunk/roadmap): the ideas that are
planned, being built or already shipped, with their votes. Vote there for what you want, or post
an idea of your own.

For what works today, see the [Support matrix](/docs/support-matrix); for what changed, the
[release notes](https://git.unom.io/unom/punktfunk/releases).

## Not planned, or blocked upstream

- **HDR on Mutter, KWin and Sway virtual displays.** Those compositors' virtual outputs are
  SDR-only upstream. gamescope, Hyprland and the GNOME 50+ monitor mirror carry HDR — see
  [HDR](/docs/hdr).
- **Hosting on macOS, iOS, tvOS or Android.** They are client-only platforms.
- **HEVC 4:4:4 on AMD.** AMD's encoder can't produce it. Intel's VAAPI path has no 4:4:4 yet
  either. [PyroWave](/docs/pyrowave) carries full chroma on both.
