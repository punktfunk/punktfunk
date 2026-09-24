---
title: Reporting an Issue
description: Send a client's log to its host, export everything as one file from the web console, and attach it to a bug report.
---

Build one file with the host's log, its health checks and your client's log, and attach it to a bug
report.

> **Security problem?** Don't open a public issue — email **security@punktfunk.com**
> ([SECURITY.md](https://git.unom.io/unom/punktfunk/src/branch/main/SECURITY.md)).

## 1. Reproduce it, and keep the app open

The client keeps its log in memory and loses it when you quit. Send it from the same app run that saw
the problem, and note roughly when it happened.

## 2. Send the client's log to the host

No stream needs to be running.

| Client | Where |
| --- | --- |
| Linux, Windows, Android | The host card's menu → **Send logs to host** |
| iPhone, iPad, Mac, Apple TV | Long-press (or right-click) the host card → **Host Details…** → **Send Logs to Host** |
| Console home (Steam Deck Gaming Mode, TV, controller) | The host's options → **Send logs to host** |

The item shows only for a **paired** host that is **online** (Apple: paired is enough). It sends the
newest 4096 lines at debug detail. If the upload fails, the host's management port (TCP 47990) is
blocked — see [Ports](/docs/ports), or grab the log [by hand](#logs-without-the-console).

## 3. Export everything from the web console

Open the [web console](/docs/web-console), pick **Troubleshooting** in the sidebar, and press
**Export all**. You get one text file, `punktfunk-diagnostics-YYYYMMDD-HHMMSS.txt`, with the health
checks, the host and plugin log, and every client log the host holds.

**Read it before you post it.** Nothing is redacted: it holds host names, IP addresses and device
names.

## 4. File the issue

Open an issue at [git.unom.io/unom/punktfunk/issues](https://git.unom.io/unom/punktfunk/issues), or
ask on [Discord](https://discord.gg/wzEGg9y45z) first. Include:

- host and client versions (the console's **Host** page; the first line of each client log)
- host OS and desktop, GPU, the client device, wired or Wi-Fi
- what you did, what you expected, what happened, and when
- the export from step 3

For stutter or lag, add a [performance recording](/docs/stats#recording-a-capture-for-a-bug-report).

## Logs without the console

| Where | How |
| --- | --- |
| Linux host | `journalctl --user -u punktfunk-host` |
| Windows host | `%ProgramData%\punktfunk\logs\` (`host.log`, `service.log`, `web.log`) from an **elevated** PowerShell |
| Windows client | **Settings → About → Diagnostics → Open log folder** |
| Mac, iPhone, iPad | Console.app on a Mac, subsystem `io.unom.punktfunk` |
| Android | `adb logcat`, tag `punktfunk` |
| Linux client | Start it from a terminal; it logs there |

These follow `RUST_LOG` (info by default), so they hold less than the uploads above, which keep
debug detail.
