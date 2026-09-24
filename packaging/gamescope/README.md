# `punktfunk-gamescope` — gamescope with 10-bit HDR PipeWire capture

Upstream gamescope's built-in PipeWire node is SDR-only: `build_format_params()` offers `BGRx`
and `NV12`, and `paint_pipewire()` hardcodes a Gamma-2.2 composite with the SDR screenshot LUT
set. An HDR game therefore reaches every capture consumer already tone-mapped down — which is
why the punktfunk gamescope backend has always streamed 8-bit, even though games *can* render
HDR on a headless gamescope today (`--hdr-enabled --hdr-debug-force-support`).

The patches here add the missing half, and nothing else. See
`punktfunk-planning/design/gamescope-hdr-virtual-output.md` for the full design.

| Patch | What | Upstream? |
|---|---|---|
| `0001-pipewire-offer-10-bit-BT.2020-PQ-capture-formats-HDR.patch` | Offer SPA `xRGB_210LE`/`xBGR_210LE` with MANDATORY SMPTE ST.2084 + BT.2020 props, map them to `DRM_FORMAT_XRGB2101010`/`XBGR2101010`, and composite them with `g_ScreenshotColorMgmtLutsHDR` + `EOTF_PQ` | **Yes** — offered against [gamescope#2126](https://github.com/ValveSoftware/gamescope/issues/2126) |
| `0002-pipewire-optionally-composite-the-cursor-into-the-ca.patch` | `--pipewire-composite-cursor` (off by default): paint the pointer into the capture stream, using the same `MouseCursor::paint` call the scanout composite uses | **Yes** — independently useful to any consumer with no cursor of its own |
| `0003-headless-advertise-the-virtual-display-s-mode-and-re.patch` | Give `CHeadlessConnector` a real `GetModes()` + `GetValidDynamicRefreshRates()` from the resolved `-W`/`-H`/`-r`, report `GAMESCOPE_SCREEN_TYPE_EXTERNAL` so `update_mode_atoms` publishes the list, and add `--custom-refresh-rates` | **Yes** — a headless session that cannot report its own mode is a plain bug |
| `0004-pipewire-optionally-composite-the-external-overlay-i.patch` | `--pipewire-composite-external-overlay` (off by default): paint the external overlay layer (mangoapp — the fps/stats readout) into the capture stream | **Yes** — same shape as the cursor patch, same argument |
| `0005-punktfunk-stamp-the-version-banner-with-pfhdrN.patch` | Append `+pfhdr<N>` to the `--version` banner | **No** — ours only, retired when the functional patches above land upstream |
| `0006-punktfunk-never-destroy-the-Vulkan-device-or-output-.patch` | Give `g_device` and `g_output` storage that is never destroyed, so their destructors cannot call a Vulkan driver glibc has already unloaded at `exit()` | **Yes** — a plain static-destruction-order bug, not punktfunk-specific |
| `0007-pipewire-never-leave-pw_buffer-user_data-pointing-at.patch` | Associate `pw_buffer->user_data` with its `pipewire_buffer` for every path out of `add_buffer`, clear it in `remove_buffer` (the last point both halves are known), and null-check the consumers — killing the use-after-free that aborted the session on every capture renegotiation | **Yes** — a plain use-after-free in the PipeWire buffer lifecycle |
| `0008-steamcompmgr-honor-GAMESCOPE_NO_FOCUS-never-a-focus-.patch` | Honor `GAMESCOPE_NO_FOCUS` (set by hhd-ui and MangoHud, consumed by nobody): such windows are skipped by both focus-candidate collectors, so a mapped-but-unpainted overlay app can no longer win focus and turn the composite black. Compositing is untouched — only focus SELECTION is barred | **Yes** — the atom's setters already exist in the wild; some compositor has to keep the promise |
| `0009-pipewire-destroy-capture-textures-on-the-compositor-.patch` | Move capture-buffer destruction off the PipeWire thread: `remove_buffer`/stale-push queue the corpse (`bury_buffer`), steamcompmgr reaps on every vblank — including while the stream is paused, which is exactly the linger window. Without it, dropping the last `CVulkanTexture` ref on the PW thread races `vulkan_screenshot` on the same device and SIGSEGVs (NVIDIA `insertBarrier`), so a lingered display is dead and reconnect loses the session. Reported + written by luxus (punktfunk-overlay#9) | **Yes** — the race is upstream's `paint_pipewire` vs `destroy_buffer`; our patches only make the paint path heavier |
| `0010-wlserver-give-the-seat-s-stub-keyboard-the-compiled-.patch` | Set the compiled keymap on `wlserver.wlr.virtual_keyboard_device` too. gamescope builds a keymap from `XKB_DEFAULT_*` but only puts it on `keyboard_group`, while the SEAT carries the keymap-less stub that `wlserver_keyboardfocus()` re-binds on every focus change — so clients get no keymap and fall back to their own `us`, and a headless session (no libinput devices) never recovers | **Yes** — the stub's own comment says it exists "only to set the keymap"; it just never did |
| `0011-headless-support-adaptive-sync-paint-on-the-game-s-c.patch` | `CHeadlessConnector` advertises VRR (`SupportsVRR()` true; `IsVRRActive()` = `cv_adaptive_sync` — no scanout cycle to honor, so "supported" and "active" collapse into "asked for"), making `--adaptive-sync` paint — and publish to PipeWire — on the game's commit instead of the synthetic vblank tick. `PacesPresents()` (new, default true, headless false) keeps the VRR framerate limiter armed when the target equals the refresh: that skip defers to a display's own pacing, and a display that paces nothing must not be deferred to | **Yes** — a headless output is the ideal adaptive-sync display (its vblank is a fiction), and the tick-quantization loss it removes is measurable by any PipeWire consumer |
| `0012-steamcompmgr-persist-the-CLI-framerate-limit.patch` | Seed `--framerate-limit` into both screen types' persistent overrides without enabling refresh switching. Later Steam requests can replace or clear the limit | **Yes** — a CLI limit must survive the refresh-policy update in every paint |
| `0013-pipewire-repaint-the-capture-on-the-Steam-overlay-s-.patch` | `paint_pipewire` pushes a frame when the Steam overlay commits, not only when the game does, and a focus window with no finished commit shows the scanout's held base (refitted to the capture size) instead of nothing | **Yes** — a game that stops presenting under the overlay left the node without the overlay, or with the overlay over black |
| `0014-pipewire-pace-the-capture-push-to-the-consumer-s-max.patch` | `paint_pipewire` pushes at most `maxFramerate` frames a second — the consumer's wire rate — and leaves a skipped change pending for the next vblank, so a game at 190 fps no longer costs two 4K composites per wire frame under adaptive sync | **Yes** — upstream pushes on every commit whatever the consumer negotiated |
| `0015-pipewire-offer-a-P010-capture-format-BT.2020-PQ-4-2-0.patch` | Offer SPA `P010_10LE` (MANDATORY BT.2020 + PQ, MANDATORY BT.2020 limited matrix) after the packed 10-bit formats, and write it with a new `cs_rgb_to_p010` pass: the PQ codes through the BT.2020 matrix into R16/RG16 plane views. An HDR consumer whose encoder takes 4:2:0 (VAAPI Main 10, Vulkan Video) then pays no conversion of its own | **Yes** — the 10-bit twin of the NV12 offer upstream already has |
| `0016-headless-report-HDR10-under-hdr-debug-force-support.patch` | With `--hdr-debug-force-support`, `CHeadlessConnector` reports an HDR10 connector (`bExposeHDRSupport`, PQ encoding, `SupportsHDR()` = `IsHDR10()`), so `GAMESCOPE_DISPLAY_SUPPORTS_HDR` and the `gamescope_control` HDR flag reach Steam. Without it Steam shows HDR as unavailable and games that follow Steam's setting cannot enable it. The composite is unchanged | **Yes** — the flag forces the WSI feedback but leaves Steam told there is no HDR |
| `0017-wlserver-bound-the-pointer-by-the-X-window-not-its-s.patch` | Bound the pointer by the input focus window's X geometry, which steamcompmgr reports on every focus pass, instead of the WSI override surface's extent. Pointer coordinates are the X window's; the override is the game's swapchain, so a swapchain smaller than the window fenced the pointer into its top-left and the right and bottom of the picture could not be reached | **Yes** — upstream bounded by window geometry before its 2024 cursor rework |
| `0018-steamcompmgr-build-capture-LUTs-from-the-live-SDR-on.patch` | Build the capture and screenshot LUT sets from the live SDR-on-HDR luminance and rebuild them when it changes. SDR maps into the BT.2020 PQ container colorimetrically (wideness 0) instead of being stretched to 2020 primaries, so Steam's UI and SDR games no longer look oversaturated in an HDR stream; `--hdr-sdr-content-nits` and Steam's SDR brightness setting now reach the stream. PQ content in an 8-bit capture rolls onto SDR white with BT.2390 instead of clipping at 500 nits | **Yes** — the static LUTs ignore every live colour setting |
| `0019-color-extrapolate-the-inverse-shaper-past-its-range.patch` | `FindLutInv` continues the shaper's last segment instead of clamping, so 3D LUT grid points above a Gamma 2.2 → PQ shaper hold real colours. SDR white in a PQ output lands at 203 nits, not 181 | **Yes** — the same LUT pair drives HDR scanout of SDR content |
| `0020-shaders-site-4-2-0-capture-chroma-on-the-left-column.patch` | `cs_rgb_to_nv12` and `cs_rgb_to_p010` filter chroma [1 2 1] across the left luma column instead of averaging the 2×2 block, so the chroma sits where H.264/HEVC/AV1 decoders place it without a signalled location (type 0). AV1 cannot signal centre siting at all | **Yes** — every decoder that assumes the default shifted capture colour edges half a pixel |
| `0021-pipewire-recycle-a-pushed-buffer-the-PipeWire-thread.patch` | `push_pipewire_buffer` returns the buffer a push displaced from the one-slot hand-off, and `paint_pipewire` paints into it next. Upstream dropped it: still dequeued and still `copying`, it left the pool until the stream renegotiated, and a pool that keeps shrinking ends in a stream that reports streaming and delivers nothing. No paint is skipped and the newest frame still goes out at once | **Yes** — upstream loses a buffer on every push that beats the PipeWire thread |
| `0022-pipewire-make-the-capture-stream-reliable-and-keep-i.patch` | Set `node.reliable` on the capture stream and start a cycle whenever `paint_pipewire` gets no buffer. On an async link the stream reclaims each buffer a cycle after sending it, so a consumer that runs late receives one it already holds and can return it only once: that buffer stays busy for good, and a few of them freeze the stream. Reliable transport (PipeWire 1.6) hands a buffer back only when the consumer returns it, but needs a driver that cycles on its own, which gamescope was not: it only cycled on a push. Also drops the per-vblank `out of buffers` error, routine on a reliable stream | **Yes** — any consumer without `RT_PROCESS` hits it |
| `0023-headless-change-the-virtual-display-s-mode-at-runtim.patch` | `GAMESCOPE_SET_OUTPUT_MODE` (three root cardinals: width, height, refresh in mHz) re-resolves a headless output's mode while the session runs — output globals, the connector's mode and refresh lists, the nested resolution when it was derived from the output, and a framerate cap still equal to the refresh being left. `GAMESCOPE_OUTPUT_MODE_FEEDBACK` reports the mode in effect. Every other backend refuses through `IBackend`'s default | **Yes** — a virtual display whose mode the caller chose is the one display that can change it without hardware |
| `0024-pipewire-keep-dmabuf-capture-buffers-in-device-memor.patch` | Ask for a CPU mapping only when the capture buffer is **not** a dmabuf. Upstream marks every capture texture `bMappable`, which allocates it `HOST_VISIBLE \| HOST_COHERENT \| HOST_CACHED` — system RAM on a discrete GPU — although only the MemFd path ever reads the mapping. Every capture composite then crosses PCIe and the consumer's import crosses it back: on an RTX 5070 Ti at 5120×1440 / 240 Hz a `vkcube` was captured at 23 fps with the GPU at 100 % and 9.6 GB/s on the bus; with the patch 240 fps at 12 % and 0.06 GB/s. An APU shares one memory, which is why it goes unnoticed there | **Yes** — any discrete-GPU consumer of the dmabuf path pays it |
| `0025-pipewire-let-the-consumer-pick-a-tiled-modifier-for-.patch` | Offer the device's tiled modifiers for the packed RGB capture formats ahead of LINEAR, with `SPA_POD_PROP_FLAG_DONT_FIXATE`, and fixate the first one the consumer kept (the two-step handshake Mutter and KWin use). `vulkan_get_capture_modifiers` keeps single-plane modifiers that support storage images and dmabuf export; `createFlags::ulDrmModifier` allocates with exactly the negotiated one. A consumer that lists LINEAR alone still gets LINEAR, every third round in a row settles on LINEAR, and the planar formats stay LINEAR | **Yes** — upstream's one-value LINEAR choice fails the link for any consumer whose default is tiled |
| `0026-pipewire-export-a-planar-capture-s-chroma-plane-wher.patch` | Hand an NV12/P010 dmabuf capture buffer out as two PipeWire data blocks on the one buffer object, plane 0 and plane 1 each at the offset and stride the driver reports, and record those subresource layouts for every LINEAR two-plane image, not only a mappable one. Upstream exported one plane with a stride from a `COLOR_BIT` query (not a valid aspect for a two-plane image) and a chunk size that assumes chroma right after the luma rows, so every consumer had to guess where chroma starts; when the driver pads the first plane, the guess reads luma bytes as chroma | **Yes** — upstream's own TODO: "support multi-planar DMA-BUF export via PipeWire" |

### Why the headless patch matters

A headless gamescope is how a streaming host gives a game a display: the caller passes the
client's exact mode and expects the session to run at it. It *does* — but it never told anyone.
`CHeadlessConnector` returned an empty span from both `GetModes()` and
`GetValidDynamicRefreshRates()` and reported `GAMESCOPE_SCREEN_TYPE_INTERNAL`, so
`update_mode_atoms()` **deleted** `GAMESCOPE_DISPLAY_MODE_LIST_EXTERNAL` (no resolution list) and
`wlserver_send_gamescope_control()` fell through to a **one-entry** refresh list built from
`g_nOutputRefresh` (no refresh list). With `-r` absent that entry is `Init()`'s 60 Hz default, so a
client on a 120 Hz panel was told its display was 60 Hz — and games capped themselves to it. Field
report 2026-08-08: "gamescope only shows 60hz and there's no other option".

### Why the cursor patch matters more than it looks

gamescope keeps the pointer out of its PipeWire node — it lives on a hardware plane for scanout —
so punktfunk has always reconstructed it from XFixes and blended it into every frame host-side.
That blend is what forces the encode path onto its compute colour-conversion arm: the zero-copy
RGB-direct encode source (`VK_VALVE_video_encode_rgb_conversion`) hands the captured buffer to a
fixed-function front end with no blend stage. Painting the cursor into the node removes the reason
for the blend, and with it a full-frame pass per frame — a gamescope session becomes the first one
that can be genuinely zero-copy end to end.

### Why the buffer-lifetime patch is the one that made sessions unusable

Patch 0007 is not a refinement — without it a managed gamescope session dies on essentially every
client connect. The host sets the session to the client's mode, the mode change renegotiates the
PipeWire stream, and the renegotiation is exactly what trips upstream's dangling
`pw_buffer->user_data`. The signature to recognise:

```
punktfunk-gamescope: ../src/pipewire.cpp:88: void destroy_buffer(pipewire_buffer*):
  Assertion `false' failed.
#5  destroy_buffer(pipewire_buffer*).cold
```

It is a use-after-free wearing an `assert(false); // unreachable` as a disguise — `buffer->type`
is read out of freed memory and falls off the end of the `switch`. A zeroed slot gives the SIGSEGV
variant of the same fault instead. Two traps when triaging it:

* **It is not HDR-specific.** The abort was first seen right after a 10-bit stream negotiated, so
  it looked like the HDR path and `PUNKTFUNK_GAMESCOPE_HDR=0` looked like a workaround. It is not
  — the same abort reproduces on an SDR session with no `--hdr-enabled` in the command line.
  Check the failing process's actual argv before believing an HDR association.
* **`gamescope-session-plus` hides it.** When our binary crash-loops, the session script retries
  and eventually comes up on the *stock* `/usr/bin/gamescope` at its default 1920×1080. So the box
  lands in a working-looking game mode at the wrong resolution and without any of these patches.
  Read the banner in `~/.gamescope-stdout.log`, not the fact that a session exists.

### Why the teardown patch is what makes linger real

Patch 0007 keeps a session alive across renegotiations; patch 0009 keeps it alive across
*disconnects*. When the capture consumer leaves, `stream_handle_remove_buffer` used to destroy
idle buffers on the PipeWire thread — and `~CVulkanTexture` talks to the Vulkan device
(`vkDestroyImage`/`FreeMemory`/dmabuf fds) while steamcompmgr can still be inside
`vulkan_screenshot` on another buffer of the same 4-buffer pool. On NVIDIA that races to a SIGSEGV
in `CVulkanCmdBuffer::insertBarrier`, timed precisely at stream end — so the display the host
keeps lingering for a reconnect is already dead, and the "resumed" session silently becomes a
fresh compositor with the game lost. The journal signature: a linger line, then a coredump, then
`kept display was dead — recreating`. Found, fixed and proven live by luxus
([punktfunk-overlay#9](https://github.com/luxus/punktfunk-overlay/issues/9)) on 4K60 HDR + composited
cursor, the heaviest paint path we ship.

## Why the marker exists

punktfunk decides a session's shape **before** the virtual display exists: the bit depth at
handshake time (irrevocable — a PQ stream handed to an 8-bit encoder is a deliberate hard error),
and whether the host must composite the cursor before the encoder is even opened. Both answers
must therefore be static properties of the resolved binary, not optimistic negotiations. The host
runs `<gamescope> --version` once per boot and reads the revision — see `gamescope_patch_level()`
in `crates/pf-vdisplay/src/vdisplay/linux/gamescope/discovery.rs`.

The number is a **monotonic patch-set revision**, so one probe answers every capability:

| Level | Adds |
|---|---|
| `+pfhdr1` | 10-bit BT.2020/PQ capture formats |
| `+pfhdr2` | …and `--pipewire-composite-cursor` |
| `+pfhdr3` | …and the headless connector advertises its mode + `--custom-refresh-rates` |
| `+pfhdr4` | …and `--pipewire-composite-external-overlay` |
| `+pfhdr5` | …and the PipeWire buffer use-after-free is fixed (no new capability) |
| `+pfhdr6` | …and `GAMESCOPE_NO_FOCUS` windows are never focus candidates (no new capability) |
| `+pfhdr7` | …and PipeWire teardown cannot SIGSEGV a lingering compositor (no new capability) |
| `+pfhdr8` | …and the seat's keyboard carries the `XKB_DEFAULT_*` keymap, so the session follows the box's configured layout |
| `+pfhdr9` | …and headless adaptive sync paints on game commits, with software pacing at the refresh rate |
| `+pfhdr10` | …and `--framerate-limit` persists across paints without changing refresh; Steam can still replace or clear it |
| `+pfhdr11` | …and the Steam overlay repaints the capture on its own commits (no new capability) |
| `+pfhdr12` | …and the capture push honours the consumer's `maxFramerate` |
| `+pfhdr13` | …and a `P010` capture format (BT.2020 PQ, 4:2:0) |
| `+pfhdr14` | …and the headless connector reports HDR10 under `--hdr-debug-force-support`, so Steam's HDR setting can be turned on |
| `+pfhdr15` | …and the pointer reaches the whole window of a game whose swapchain is smaller (no new capability) |
| `+pfhdr16` | …and capture maps SDR into BT.2020 colorimetrically at the live SDR-on-HDR luminance, with left-sited 4:2:0 chroma (no new capability) |
| `+pfhdr17` | …and a capture push never drops a buffer the PipeWire thread has not taken (no new capability) |
| `+pfhdr18` | …and the capture stream is reliable and cycles while it waits for its consumer (no new capability) |
| `+pfhdr19` | …and `GAMESCOPE_SET_OUTPUT_MODE` changes a headless output's mode while the session runs, so one kept compositor serves any client mode |
| `+pfhdr20` | …and dmabuf capture buffers live in device memory, so a discrete GPU stops moving every captured frame across PCIe (no new capability) |
| `+pfhdr21` | …and a packed RGB capture dmabuf takes a tiled modifier its consumer accepts; the host offers them from this level |
| `+pfhdr22` | …and a planar (NV12/P010) capture dmabuf exports its chroma plane where the driver put it (no new capability: the host reads a second data block whenever one is there) |

Require `+pfhdr10` for headless `--adaptive-sync` with a CLI cap: `+pfhdr9` clears that cap
on the first paint unless Steam or a control command supplies an override. The Arch package is
`3.16.25.pfhdr10-1`; its build checks the complete upstream version and capability level.

Bump it whenever a patch adds or changes something the host must know about before it spawns.

A patch that only fixes a crash does **not** automatically bump it: `0006` (the exit-time Vulkan
teardown fix) changes nothing the host probes for, so it shipped as a `pkgrel` bump at `+pfhdr4` —
exactly the split the PKGBUILD's own comment describes. Since every host probe is `>=`, a bump for
a bugfix is safe but must earn its place: `0007` and `0008` moved the level anyway because their
absence is invisible until a stream fails (a crash-loop per connect; a composite lost to a
NO_FOCUS window), so field triage has to be able to read the difference off a box's banner.
Bumping without either reason would advertise a capability tier that does not exist.

⚠️ The two indirect spawn modes (the `GAMESCOPE_BIN` wrapper for gamescope-session-plus, and the
SteamOS PATH shim) pass these flags through `PF_HDR_ARGS`, so they share one dependency: if the
session ignores `GAMESCOPE_BIN`/`PATH` and execs the distro's gamescope, it gets neither the HDR
formats nor the cursor flag. HDR fails loudly there (the capture negotiation times out and latches
an SDR downgrade) — but a missing cursor would be silent, because the host was told the compositor
would paint the pointer and so painted none itself.

Both managed paths therefore **verify after spawn**: once the session's node appears,
`verify_managed_spawn_flags` reads the running compositor's `/proc/<pid>/cmdline` and refuses the
session if a flag we passed isn't there. The plan is fixed by then (`cursor_blend` feeds the encoder
open, which precedes the display), so the session cannot be corrected in place — instead the
capability is latched off for the process and the spawn fails, and the retry resolves a correct SDR
host-composited session. One rejected attempt per boot, then it converges.

The check fails **open** at every ambiguity: no flags expected, or no readable gamescope in
`/proc`, says nothing. Only a compositor we can see, missing a flag we can name, fails.

## Which binary the host runs

Resolution order, applied identically by the bare spawn, the `GAMESCOPE_BIN` wrapper
(gamescope-session-plus) and the SteamOS PATH shim:

1. `PUNKTFUNK_GAMESCOPE_BIN` — absolute path override
2. `punktfunk-gamescope` on `PATH`
3. `gamescope`

So installing this build under the name `punktfunk-gamescope` is enough; nothing replaces the
distro's `gamescope`.

## Building

Pinned upstream: `5fb8dce4a09d0a68d097b9faf9513782106bc843` (`3.16.25-11-g5fb8dce`).
All patches apply in filename order to that commit.

The bump from `8c676c39` is deliberate: it brings upstream's `vulkan_get_rgb10_capture_format()`
(`ff6b924`), which probes `linearTilingFeatures` for STORAGE+SAMPLED and falls back to
`DRM_FORMAT_XBGR2101010` on devices that cannot do linear-tiled `A2R10G10B10` — i.e. every
NVIDIA. That covers the paths that are upstream's rather than ours: the RGB intermediate
`paint_pipewire()` acquires when the stream is YCbCr, and AVIF screenshots. Our own 10-bit RGB
node is covered by patch `0001`, which offers `xBGR_210LE` first for the same reason.

Build into a staging directory on Linux. The system gamescope and file capabilities stay untouched:

```sh
PF=/absolute/path/to/punktfunk
STAGE="$PWD/gamescope-stage"
bash "$PF/packaging/gamescope/build-punktfunk-gamescope.sh" \
    --destdir "$STAGE" --prefix /usr --jobs "$(nproc)"
"$STAGE/usr/bin/punktfunk-gamescope" --version
```

The script requires the exact current capability marker before staging the binary. To retain
build logs, pass `--srcdir /path/to/gamescope` with a checkout at the pinned commit and its
submodules initialized. A reused checkout with an older marker is rejected; use a fresh checkout
to upgrade the patch set.

The package's banner regression cases run without a compiler or `makepkg`:

```sh
bash -c 'source "$1/packaging/gamescope/PKGBUILD"; check' _ "$PF"
```

### Build dependencies

They are gamescope's, not ours, and they vary by distro. Two shortcuts that work:

```sh
# Fedora / Bazzite (inside a toolbox/distrobox — the host is immutable)
sudo dnf install -y dnf-plugins-core meson ninja-build glslc
sudo dnf builddep -y gamescope
sudo dnf install -y xorg-x11-server-Xwayland-devel      # NOT pulled by builddep; wlroots needs it
sudo dnf install -y libstdc++-static                    # NOT pulled by builddep; see below

# Arch / SteamOS — see the makedepends in ./PKGBUILD
```

⚠️ `dnf builddep gamescope` resolves Fedora's *packaged* gamescope, which is older than the master
we pin, so it can come up short. `xorg-x11-server-Xwayland-devel` is the one that actually bit
(2026-07-28, Fedora 43): without it wlroots' configure fails with `Neither a subproject directory
nor a xserver.wrap file was found`, several minutes into an otherwise clean run. If a different
one surfaces, meson names it — install and re-run with `--srcdir` so the clone is not repeated.

⚠️ `libstdc++-static` is a **punktfunk** requirement, not gamescope's, so no builddep will ever pull
it: the build script links the C++ runtime statically on purpose (see the long comment beside
`LDFLAGS` in `build-punktfunk-gamescope.sh`). Without it meson fails at configure with a message
that names neither the flag nor the package (2026-08-10, Fedora 44):

```
ERROR: Compiler c++ cannot compile programs.
  /usr/bin/ld.bfd: cannot find -lstdc++
```

The cleanup trap deletes the temp checkout on failure, taking `meson-logs/meson-log.txt` with it —
so build with `--srcdir` when diagnosing, or the evidence is gone before you can read it.

`gamescope` needs `CAP_SYS_NICE` for its realtime priority; the distro packages set it on their
own binary. Mirror it if you install ours system-wide:

```sh
setcap 'CAP_SYS_NICE=eip' /usr/bin/punktfunk-gamescope
```

## How each channel ships it

Four packaging paths, one build recipe — `build-punktfunk-gamescope.sh` — because the two things
that are easy to get wrong (which patches get applied, and whether wlroots is linked statically)
must not be decided twice.

| Channel | Built by | Notes |
|---|---|---|
| Bazzite / Fedora Atomic | `.gitea/workflows/rpm.yml` → `build-sysext.sh --gamescope` | Inside the matching Fedora container, per major — the binary is soname-coupled to its base exactly like the RPM |
| Arch / SteamOS | `.gitea/workflows/arch.yml` → `makepkg` on `./PKGBUILD` | Its own pkgbase in the same pacman repo; `pacman -S punktfunk-gamescope` |
| NixOS | `packaging/nix/gamescope.nix` (an `overrideAttrs` on nixpkgs' gamescope) | The one path that does NOT call the script — nixpkgs already solves the submodules, and a nix closure names every library it links |
| Anything else | the script, by hand | See *Building* above |

Both CI builds are **cached on `packaging/gamescope/**`** and **best-effort**. Cached because this
tree depends on nothing else in the repo, so a normal push restores a binary instead of spending
ten minutes on someone else's C++; best-effort because punktfunk works without it (SDR on the
gamescope backend, which is what every release before this one did) and a hiccup building gamescope
must not cost the packages those workflows exist to publish. A failed build emits a `::warning::`
and is never cached, so the next run retries.

Note what is NOT in that table: the `.deb`. Debian/Ubuntu boxes build it by hand for now.

## Verifying the patch on a box (P0 exit)

```sh
punktfunk-gamescope --version                    # version token ends in +pfhdr10
punktfunk-gamescope --backend headless -W 1920 -H 1080 -r 60 \
    --hdr-enabled --hdr-debug-force-support --pipewire-composite-cursor -- vkcube &
pw-dump | grep -A40 '"gamescope"'                # node offers xRGB_210LE / xBGR_210LE
```

The stream is only 10-bit once a **consumer** asks for it: the formats are listed last, so any
consumer that negotiates the 8-bit stream today keeps negotiating it bit-for-bit.

### Headless VRR limiter regression

With Linux Vulkan, Xwayland and Vulkan-tools' `vkcube`, run the staged binary without Steam or
`gamescopectl`. Both runs must take roughly four seconds plus startup for 240 FIFO frames at
60 FPS. A sub-3.5-second run fails the cap check. This checks pacing, not capture latency.
The WSI layers are disabled so an installed layer cannot mask the compositor's limiter behavior.

```sh
python3 - "$STAGE/usr/bin/punktfunk-gamescope" <<'PY'
import os
import subprocess
import sys
import time

env = dict(os.environ, DISABLE_GAMESCOPE_WSI="1", PUNKTFUNK_GAMESCOPE_WSI_DISABLE="1",
           MESA_VK_WSI_PRESENT_MODE="fifo")
for adaptive_sync in (False, True):
    args = [sys.argv[1], "--backend", "headless", "-W", "1280", "-H", "720",
            "-r", "60", "--framerate-limit", "60"]
    if adaptive_sync:
        args.append("--adaptive-sync")
    start = time.monotonic()
    subprocess.run(args + ["--", "vkcube", "--present_mode", "2", "--c", "240"],
                   env=env, check=True, timeout=30)
    elapsed = time.monotonic() - start
    print(f"adaptive_sync={adaptive_sync}: {elapsed:.2f}s")
    if elapsed < 3.5:
        raise SystemExit("FAIL: the CLI framerate limit did not pace FIFO presents")
PY
```

## Rebase policy

The functional patch is two files and mirrors code that already exists in-tree (the HDR AVIF
screenshot path), so it rebases cheaply. We pin the gamescope commit we ship; when upstream
takes it, both patches are dropped and the host's capability probe becomes a plain version
floor.
