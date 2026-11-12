---
title: Uninstalling the Windows Host
description: Remove the Punktfunk host from a Windows PC, clear what the uninstaller keeps on purpose, and clean up a display or gamepad an older build left behind.
---

Remove the Punktfunk host from a Windows PC and clear what the uninstaller leaves behind.

## Windows host

Uninstall **Punktfunk Host** from **Settings → Apps → Installed apps**, or run
`winget uninstall unom.PunktfunkHost`. Both remove the service, the drivers, the scheduled tasks and
the firewall rules ([full list](/docs/windows-host#uninstalling)). Left behind on purpose:

- **`%ProgramData%\punktfunk`**, your config and pairings:
  `Remove-Item -Recurse -Force "$env:ProgramData\punktfunk"`.
- **The winget source**, if you added it: `winget source remove -n punktfunk` (admin PowerShell).
- **Seat accounts**, `pf_seat1` to `pf_seat4`, if you used [seats](/docs/windows-seats). Remove
  their profiles under **Profiles** before you uninstall, or delete the accounts in **Computer
  Management** → **Local Users and Groups** afterwards.
- **VB-CABLE**, if an older Punktfunk installed it. Other apps may use it; remove it with its own
  uninstaller (**VB-Audio Virtual Cable** in Installed apps).
- **The old unom certificate**, if you imported it by hand for release 0.28.1 or earlier. Current
  releases don't need it: in `certlm.msc`, delete the certificate issued to **unom** (thumbprint
  `CD1EFDEEEC9743AFC38F56C5AF30C5A3009BE941`) from **Trusted Publishers** and **Trusted Root
  Certification Authorities**.

### A Punktfunk display or gamepad stays in Device Manager

An older build could leave one behind. While the host is still installed, run from an elevated
prompt:

```powershell
punktfunk-host driver uninstall
punktfunk-host driver uninstall --gamepad
punktfunk-host driver uninstall --audio
```

Already uninstalled? Install the current version and uninstall it again; its uninstaller runs all
three.
