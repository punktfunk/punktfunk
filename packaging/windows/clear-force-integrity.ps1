# Clear the PE FORCE_INTEGRITY bit (IMAGE_DLLCHARACTERISTICS_FORCE_INTEGRITY = 0x0080) from a driver DLL.
#
# windows-drivers-rs / wdk-build links UMDF drivers with /INTEGRITYCHECK (sets the bit) UNCONDITIONALLY
# (wdk-build configure_binary_build -> cargo::rustc-cdylib-link-arg=/INTEGRITYCHECK; no opt-out). With the
# bit set, Windows Code Integrity refuses to load a binary whose signature doesn't chain to a Microsoft
# root (errors 3004/3089) - so a SELF-SIGNED driver won't load. Clearing the bit (then re-signing) lets a
# self-signed driver load under Secure Boot. This is the deterministic, idempotent step every
# punktfunk driver build runs.
#
# Order in the packaging flow: cargo build -> THIS -> signtool (sign .dll) -> Inf2Cat (.cat) -> sign .cat.
# (Clearing AFTER signing would invalidate the signature; clear FIRST, then sign.)
#
# DllCharacteristics lives at OptionalHeader+0x46 = (e_lfanew + 24) + 0x46 = e_lfanew + 0x5E for both PE32
# and PE32+ (the fields up to it share offsets).
[CmdletBinding()]
param([Parameter(Mandatory)][string]$Path)
$ErrorActionPreference = 'Stop'

if (-not (Test-Path $Path)) { throw "clear-force-integrity: file not found: $Path" }
$b = [IO.File]::ReadAllBytes($Path)
if ($b.Length -lt 0x40 -or $b[0] -ne 0x4D -or $b[1] -ne 0x5A) { throw "not a PE (no 'MZ'): $Path" }
$pe = [BitConverter]::ToInt32($b, 0x3C)
if ($pe -le 0 -or $pe + 0x60 -ge $b.Length -or $b[$pe] -ne 0x50 -or $b[$pe + 1] -ne 0x45) {
  throw "no 'PE\0\0' signature at e_lfanew=$pe in $Path"
}
$off = $pe + 0x5E
$FORCE_INTEGRITY = 0x0080
$dllchar = [BitConverter]::ToUInt16($b, $off)

if (($dllchar -band $FORCE_INTEGRITY) -eq 0) {
  Write-Host ("clear-force-integrity: already clear (DllCharacteristics=0x{0:X4}) - no change: $Path" -f $dllchar)
} else {
  $new = [uint16]($dllchar -band (-bnot $FORCE_INTEGRITY))
  [BitConverter]::GetBytes($new).CopyTo($b, $off)
  [IO.File]::WriteAllBytes($Path, $b)
  Write-Host ("clear-force-integrity: cleared FORCE_INTEGRITY 0x{0:X4} -> 0x{1:X4} in $Path" -f $dllchar, $new)
}

# Verify on disk (re-read) - the assertion.
$v = [BitConverter]::ToUInt16([IO.File]::ReadAllBytes($Path), $off)
if (($v -band $FORCE_INTEGRITY) -ne 0) { throw ("FORCE_INTEGRITY still set after clear (0x{0:X4})" -f $v) }
Write-Host ("clear-force-integrity: verified DllCharacteristics=0x{0:X4}, FORCE_INTEGRITY clear." -f $v)
