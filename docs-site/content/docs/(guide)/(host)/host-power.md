---
title: Host power
description: Sleep, restart or shut down the host from the web console or a paired client, who may do it, and why an action can be refused.
---

Sleep the host from the couch when you're done, and [wake it](/docs/wake-on-lan) tomorrow.
**Sleep host**, **Restart host** and **Shut down host** work on Linux and Windows hosts.

- **Web console:** **Host** → **Host power**. Each action asks for your console password.
- **Paired client:** the host's menu, where **Wake host** appears when it sleeps. The rows appear
  only for a device with the **Host power** grant. Restart and shut down ask before they run.

## Who may do it

- The **web console** always can.
- A **paired device** needs the **Host power** grant ([access levels](/docs/access-levels)). Full
  control includes it; Controller only and View only don't.

## What happens

The host ends every stream first, and clients say the host is going to sleep or shutting down. It
removes its virtual displays too, so a wake starts clean. A display kept until released stays, so
a gamescope game survives the sleep. If a wake then shows a black screen, click **Release** in
**Displays**.

## Why an action can be refused

The host refuses with the reason:

- **Another device is streaming.** A guest can't pull the host out from under someone else's
  session. Your own session never blocks you, and the console is never blocked.
- **The system said no.** A Linux host respects other programs' sleep inhibitors and refuses while
  another user is logged in locally. A machine that can't sleep lists **Sleep host** as
  unavailable, with the reason.
- **Linux: the host user isn't in group `punktfunk`.** The packages let that group sleep, restart
  and shut down the machine without a password prompt. The console shows the action as
  unavailable.

Moonlight clients have no power actions.

## Restart Punktfunk

**Restart Punktfunk** on the same card restarts the host service, not the machine. Streams end the
same way and the service comes straight back. The console offers it when a
[setting](/docs/configuration#settings-in-the-web-console) applies only after a restart.

It needs a service to start the host again: `punktfunk-host` as a user service on Linux, the
Punktfunk Host service on Windows. A host started by hand in a terminal shows it as unavailable.
