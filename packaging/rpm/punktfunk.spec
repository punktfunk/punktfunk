################################################################################
# punktfunk — low-latency desktop/game streaming host (RPM for Fedora / Bazzite)
#
# Builds `punktfunk-host` from source with cargo and installs the binary, the
# uinput udev rule (virtual gamepads), the systemd *user* unit, and the headless
# session helpers. Designed for COPR (build-from-SCM): COPR clones the repo and
# runs this spec; `cargo build` fetches crates over the network (COPR allows it).
#
# DEPENDENCIES NOT IN BASE FEDORA:
#   * The NVIDIA driver (libnvidia-encode / libEGL_nvidia) — present on Bazzite's
#     -nvidia images; on plain Fedora install akmod-nvidia + xorg-x11-drv-nvidia-cuda.
#   * mesa-va-drivers-freeworld (RPM Fusion) for full AMD/Intel VAAPI encode — Fedora's
#     stock driver has HEVC and AV1 disabled.
#
# Bazzite already ships gamescope, PipeWire and the NVIDIA stack, so on Bazzite the
# only new runtime bits are opus + libei.
################################################################################

Name:           punktfunk
# Version/Release are overridable so CI can stamp a rolling snapshot: a canary main build passes
#   --define "pf_version 0.3.0" --define "pf_release 0.ci42.gdeadbee"
# (Release starting "0." sorts BEFORE the eventual "1" release; the canary base stays one minor
# ahead of the latest stable), a vX.Y.Z release tag passes the clean version with "pf_release 1".
# A plain `rpmbuild` (or COPR) with no defines builds 0.3.0-1.
Version:        %{?pf_version}%{!?pf_version:0.3.0}
Release:        %{?pf_release}%{!?pf_release:1}%{?dist}
Summary:        Low-latency desktop/game streaming host (Moonlight-compatible + punktfunk/1)

License:        MIT OR Apache-2.0
URL:            https://git.unom.io/unom/punktfunk
# COPR SCM builds provide the checkout; for a tarball build, drop a git archive here:
Source0:        %{name}-%{version}.tar.gz

# punktfunk-host is Linux-only and links system PipeWire/Opus. The HOST is x86_64 only —
# its encode stack is NVENC/QSV/AMF — but the CLIENT builds and runs fine on aarch64, so the spec
# accepts both arches and `--without host` (below) selects the client-only build.
ExclusiveArch:  x86_64 aarch64

# The zerocopy FFI links the NVIDIA driver's libcuda.so.1; rpm's auto-dep generator would turn
# that into a hard Requires on libcuda.so.1 (and we never want to pin the driver — NVENC/EGL come
# from whatever NVIDIA stack the host runs, expressed below as the weak xorg-x11-drv-nvidia-cuda
# Recommends). Drop it from the auto-Requires, mirroring the Debian package's NVIDIA filter.
%global __requires_exclude ^libcuda\\.so.*$

# Management web console subpackage (punktfunk-web). OFF by default: building the Nitro SSR bundle
# (and running it) needs `bun`, which a plain rpmbuild / COPR mock chroot does NOT have. CI's builder
# image (ci/fedora-rpm.Dockerfile) DOES have bun and builds with `--with web`, so the Gitea RPM
# registry carries punktfunk-web. COPR (no bun) builds host+client only — use the Gitea registry for
# the console, or enable bun + `--with web` in the COPR project. Mirrors the Debian punktfunk-web .deb.
%bcond_with web

# Plugin/script runner subpackage (punktfunk-scripting). OFF by default for the same reason as web:
# building the bun bundle needs `bun`, absent from a plain rpmbuild / COPR mock chroot. CI's builder
# image has bun and builds with `--with scripting`, so the Gitea RPM registry carries it. Mirrors the
# Debian punktfunk-scripting .deb.
%bcond_with scripting

# The HOST half of this spec (the punktfunk package itself + the tray). ON by default, so an
# ordinary x86_64 build is unchanged. `--without host` drops the host binary, the tray, the
# headless-session data, the firewalld services and the main %%files section entirely, leaving
# only punktfunk-client — which is what an aarch64 build produces, since the host's encode stack
# (NVENC/QSV/AMF) is x86 and the client's is not. Omitting the main %%files is what stops rpm
# from emitting an empty `punktfunk` package alongside the client.
%bcond_without host

# --- Build toolchain ---------------------------------------------------------
BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  gcc
BuildRequires:  gcc-c++
BuildRequires:  clang
BuildRequires:  clang-devel
BuildRequires:  cmake
BuildRequires:  nasm
BuildRequires:  pkgconfig
BuildRequires:  systemd-rpm-macros
# Link-time system libraries (the -sys crates probe these via pkg-config):
BuildRequires:  pkgconfig(libpipewire-0.3)
BuildRequires:  pkgconfig(libspa-0.2)
BuildRequires:  pkgconfig(wayland-client)
BuildRequires:  pkgconfig(xkbcommon)
BuildRequires:  pkgconfig(opus)
# Zero-copy GPU path: src/zerocopy/ links libGL + libgbm (mesa) via hand-rolled FFI.
BuildRequires:  pkgconfig(gl)
BuildRequires:  pkgconfig(gbm)
# The client subpackage (GTK4 shell + SDL3 gamepads + the Vulkan session streamer).
BuildRequires:  pkgconfig(gtk4)
BuildRequires:  pkgconfig(libadwaita-1)
BuildRequires:  pkgconfig(sdl3)
# No vulkan-headers BuildRequires: the only crate that ever needed the Vulkan C headers was the
# client's pf-ffvk, whose bindgen ran over FFmpeg's libavutil/hwcontext_vulkan.h — and M10 deleted
# it with the rest of the client's FFmpeg. Nothing has replaced that need: pf-vkdecode/pf-presenter
# reach Vulkan through ash (loader dlopen'd, no headers), and pyrowave-sys builds against its own
# vendored copy (crates/pyrowave-sys/build.rs).
# The HOST links the NVIDIA CUDA driver lib (-lcuda) via FFI, so libcuda.so must be present
# at LINK time. A normal NVIDIA host (or Bazzite -nvidia) has it; a headless COPR/koji builder
# without a GPU does NOT — point %build at the CUDA toolkit stub (…/stubs/libcuda.so) there,
# e.g. `ln -s $(rpm -ql cuda-cudart-devel | grep stubs/libcuda.so | head -1) /usr/lib64/`.
# (Proper fix tracked separately: make the cuda/gbm/GL FFI dlopen-based like khronos-egl.)

# --- Runtime -----------------------------------------------------------------
Requires:       pipewire
Requires:       wireplumber
# The host captures the sink monitor through NATIVE PipeWire (audio/linux.rs) and never opens a
# Pulse socket itself — the shim is for the GAMES, which commonly emit through the PulseAudio
# API. Weak-dep, because `pipewire-pulseaudio` CONFLICTS with `pulseaudio`: as a hard Requires it
# made the host uninstallable for anyone running real PulseAudio, which serves those games just
# as well. Fedora installs pipewire-pulseaudio by default, so the default box is unaffected.
Recommends:     pipewire-pulseaudio
# The data-plane threads renice themselves through RealtimeKit when the direct setpriority() is
# refused (thread_qos — the host binary can never carry CAP_SYS_NICE, see the %%files note).
# Weak-dep: Fedora desktops ship rtkit anyway, and without it the user@.service.d LimitNICE
# drop-in below still covers the direct path from the next login.
Recommends:     rtkit
Requires:       opus
Requires:       libei
# A compositor to drive. Bazzite ships gamescope; the others are user choice.
Recommends:     gamescope
Suggests:       kwin
Suggests:       mutter
# NVENC + GPU EGL come from the NVIDIA driver; on Bazzite the -nvidia image has it.
Recommends:     (xorg-x11-drv-nvidia-cuda if xorg-x11-drv-nvidia)
# VAAPI encode drivers for AMD (radeonsi) / Intel (iHD) — the auto-selected VAAPI backend on a
# non-NVIDIA GPU. NOTE: Fedora's stock mesa-va-drivers has HEVC/AV1 *disabled* (patents); full
# encode needs mesa-va-drivers-freeworld from RPM Fusion.
Recommends:     mesa-va-drivers
Recommends:     intel-media-driver
# The management web console (pairing + status) every user needs — a separate noarch subpackage.
# Weak-dep so `dnf install punktfunk` pulls it where it exists (the Gitea registry); harmless where
# it doesn't (a COPR build without `--with web` simply has no punktfunk-web to satisfy).
Recommends:     punktfunk-web
# The plugin/script runner (host automation on bun). Same weak-dep story: pulled where it exists,
# harmless where a `--with scripting`-less build didn't produce it. Its systemd --user unit ships
# disabled — the runner is inert until you add scripts/plugins.
Recommends:     punktfunk-scripting

%description
punktfunk is a Linux-first, low-latency desktop and game streaming host. It speaks
the Moonlight/GameStream protocol (pair a stock Moonlight client) and its own native
punktfunk/1 protocol (GF(2^16) Leopard FEC + AES-GCM, mid-stream mode renegotiation,
client microphone passthrough). Each session gets a virtual output at the client's
exact resolution and refresh via a per-compositor backend (KWin, gamescope, Mutter,
Sway/wlroots), captured zero-copy (dmabuf -> CUDA -> NVENC) and split-encoded above
~1 Gpix/s. Input (mouse/keyboard/gamepads) is injected back into the session.

%package client
Summary:        Low-latency desktop/game streaming client (punktfunk/1, GTK4)
# Audio playback / mic capture want the PipeWire daemon; degrade gracefully without it.
Recommends:     pipewire
Recommends:     wireplumber
# The session streamer loads libvulkan at runtime (ash) for its ash/Skia presenter + Vulkan
# Video decode. vulkan-loader provides libvulkan.so.1; the ICD is the GPU's mesa/NVIDIA driver.
Requires:       vulkan-loader

%description client
The native Linux client for punktfunk. Discovers hosts on the LAN (mDNS), trusts
them via certificate pinning with a SPAKE2 PIN pairing ceremony, and streams HEVC
video (GF(2^16) Leopard FEC + AES-GCM over UDP, QUIC control plane) with Opus
audio, microphone passthrough, and full gamepad support including DualSense
touchpad, motion, adaptive triggers and lightbar through SDL3. The host creates a
virtual output at exactly this client's resolution and refresh rate — no scaling.

%if %{with web}
%package web
Summary:        punktfunk management web console (Nitro SSR on bun + React)
# Runtime is BUN (the console uses Nitro's `bun` preset + a Bun.serve TLS entry — node can't
# run it), from punktfunk-bun, pinned to this exact build. No system nodejs/bun dependency.
Requires:       %{name}-bun%{?_isa} = %{version}-%{release}

%description web
The browser console for a punktfunk streaming host: status, paired devices, and the SPAKE2
PIN pairing flow every client needs. Runs as a systemd --user service on port 3000 over HTTPS
(HTTP/1.1 over TLS, with the host's own identity cert), login-gated (a password generated on first
start), proxying the host's loopback HTTPS management API with a bearer token injected server-side
(never sent to the browser). Auto-wired to the host on a packaged install — it sources the host's
mgmt token, identity cert, and a generated login password, no env editing. Runs on the bun from
punktfunk-bun. Enable with `systemctl --user enable --now punktfunk-web`.
%endif

%if %{with scripting}
%package scripting
Summary:        punktfunk plugin/script runner (Effect SDK on bun)
# Each plugin runs in its own bwrap sandbox; without it the runner starts no plugin at all.
Requires:       bubblewrap
# Runtime is BUN — the runner import()s the operator's .ts plugin files, which only bun can do —
# from punktfunk-bun, pinned to this exact build. The runner itself is bundled to ONE
# self-contained JS (effect + SDK inlined), so no node_modules ship.
Requires:       %{name}-bun%{?_isa} = %{version}-%{release}

%description scripting
The plugin/script runner for a punktfunk streaming host: it discovers loose scripts under
~/.config/punktfunk/scripts and installed punktfunk-plugin-* packages under ~/.config/punktfunk/
plugins, and supervises each as an Effect fiber (capped-jittered restart; SIGTERM shuts the whole
tree down structurally so plugin finalizers run). A plugin auto-wires to the host's mgmt token +
identity cert on the same box — no env editing. Runs on the bun from punktfunk-bun. ON BY
DEFAULT: the systemd --user unit is enabled for every user (systemctl --global). The game-library
scanners ship as plugins, so a host without the runner has an empty library. Opt out per user with
`systemctl --user mask punktfunk-scripting`.
%endif

%if %{with web} || %{with scripting}
%package bun
Summary:        Bun runtime for punktfunk-web and punktfunk-scripting
# Bun isn't in the Fedora repos, so the build env's bun is vendored here once for both consumers.
# A private libexec dir, never on PATH, so it never collides with a system-wide bun. Arch-specific.

%description bun
The bun runtime the punktfunk web console and plugin runner run on, installed once at
/usr/libexec/punktfunk-bun/bun. It is not on PATH and does not replace a system-wide bun.
%endif

%prep
%autosetup -n %{name}-%{version}

%build
# Release build of the host + client binaries (the workspace also has the core lib).
# cargo fetches crates over the network; COPR build hosts allow this.
export RUSTFLAGS="%{?build_rustflags}"
# Use the toolchain baked into the builder image as-is, ignoring rust-toolchain.toml. The toml
# floats `channel = "stable"` and requests rustfmt/clippy (lint-only — not needed for a build); when
# a newer stable lands upstream, that combination makes rustup try to UPDATE the baked, minimal-
# profile `stable` toolchain in place, and the in-image OverlayFS rejects the staging rename with
# EXDEV ("Invalid cross-device link"), failing %build. RUSTUP_TOOLCHAIN bypasses the toml so rustup
# neither re-resolves the channel nor adds components — it just builds with what's installed.
export RUSTUP_TOOLCHAIN=stable
# Stamp the exact NVR into the binary for --version / mgmt /health provenance (build.rs reads it).
export PUNKTFUNK_BUILD_VERSION="%{version}-%{release}"
# --locked: reproducible from (commit + Cargo.lock), matching the .deb build path.
# punktfunk-client-session is the Vulkan/Skia streamer the shell execs for a connect — both
# client binaries must ship or streaming from the desktop client breaks.
# --features punktfunk-host/nvenc: the direct-SDK NVENC path (real RFI + recovery anchor on Linux
# NVIDIA; design/linux-direct-nvenc.md). AMD/Intel-safe — the NVENC/CUDA entry points are dlopen'd
# at runtime (no link-time dep; __requires_exclude already drops libcuda), so the binary starts
# driver-less; the encoder engages only on a CUDA frame, and the `cuda` gate keeps AMD/Intel on
# VAAPI regardless. Without this feature an NVIDIA box has no NVENC at all.
# --features punktfunk-host/vulkan-encode: the AMD/Intel twin — a raw VK_KHR_video_encode_h265 backend
# with real RFI (clean P-frame recovery anchor via DPB reference slots; design/linux-vulkan-video-encode.md).
# Pure Rust `ash` (no new lib / no link-time dep); default on for HEVC (PUNKTFUNK_VULKAN_ENCODE=0 opts
# back to the native VAAPI session), and a failed open falls back to it so unsupported devices
# degrade gracefully.
# -p punktfunk-encode-worker: the capability-carrying PyroWave encode worker, shipped next to the
# host in %%{_bindir} and granted cap_sys_nice=ep via %%caps in %%files. It MUST be a separate file
# (the host can never carry a capability — KWin identification, see the note in %%files), and it
# must ship in the SAME package: host and worker version-check each other over their socket and
# fall back to the in-process encoder on any mismatch. Co-built in this one invocation on purpose —
# v1 accepts that the worker shares the host's dependency graph (same package, same sonames, no
# new break class), so cargo's feature unification here is harmless.
%if %{with host}
cargo build --release --locked --features punktfunk-host/nvenc,punktfunk-host/vulkan-encode \
  -p punktfunk-host -p punktfunk-encode-worker \
  -p punktfunk-client-linux -p punktfunk-client-session -p punktfunk-cli \
  -p pf-update
%else
# Client-only (aarch64): no host crate, so none of the encode features apply. pf-update still
# builds — the client subpackage ships its own copy for `punktfunk-client --apply-update`.
cargo build --release --locked -p punktfunk-client-linux -p punktfunk-client-session \
  -p punktfunk-cli -p pf-update
%endif
# The status tray in its OWN cargo invocation — load-bearing, not tidiness. Cargo unifies features
# across everything in one build, so co-building the tray with the host pulls the host's
# ashpd -> zbus/tokio onto the tray's shared zbus; the tray (ksni async-io + blocking, no tokio
# runtime by design) then panics at startup ("there is no reactor running, must be called from the
# context of a Tokio 1.x runtime"). Built alone, its zbus stays on async-io. (Same split the .deb does.)
%if %{with host}
cargo build --release --locked -p punktfunk-tray
%endif

%if %{with web}
# Management web console: the Nitro SSR bundle (the `bun` preset + our Bun.serve TLS entry). bun is
# both the build tool AND the runtime (vendored in %%install below).
#
# `pf_prebuilt_web` (optional, absolute path to an already-built web/.output) lets CI hand over a
# bundle it has already produced instead of building a second, identical one here. It exists because
# this console was being rebuilt SIX times per push — ci.yml, deb, both RPM legs, arch, and the
# docker app image — at ~2.5 min each. rpm.yml restores it from the shared actions cache and passes
# this macro; see build-rpm.sh. Undefined (a plain rpmbuild, or COPR) takes the build path exactly
# as before, so nothing outside CI changes.
#
# It has to be a MACRO carrying an absolute path, not simply a pre-populated web/.output: this spec
# builds from the `git archive` tarball build-rpm.sh generates, and web/.output is gitignored — it
# is not in the tarball and cannot be, so there is nothing here to find without being told where to
# look.
%if %{defined pf_prebuilt_web}
echo "==> reusing the prebuilt web console from %{pf_prebuilt_web}"
mkdir -p web/.output
cp -a %{pf_prebuilt_web}/. web/.output/
%else
(cd web && bun install --frozen-lockfile && bun run build)
%endif
# Asserted for BOTH paths on purpose. This is the check that says the artifact about to be packaged
# is the bun preset (node cannot run Bun.serve) — a handed-over bundle deserves it at least as much
# as a freshly built one, because a cache is one more place a wrong artifact can come from.
if ! grep -q 'Bun\.serve' web/.output/server/index.mjs; then
  echo "ERROR: web build is not a bun bundle — need the 'bun' preset + custom entry" >&2
  exit 1
fi
%endif

%if %{with scripting}
# Plugin/script runner: bundle the SDK's runner CLI to ONE self-contained JS with bun
# (`--target=bun` inlines effect + the SDK; the dynamic plugin import stays a runtime import). bun is
# both the build tool AND the vendored runtime (in %%install below).
(cd sdk && bun install --frozen-lockfile --ignore-scripts && \
  bun build src/runner-cli.ts --target=bun --outfile=../runner-cli.js)
if ! grep -q 'attempt=' runner-cli.js; then
  echo "ERROR: runner bundle missing the dynamic plugin import — wrong build" >&2
  exit 1
fi
%endif

%install
%if %{with host}
# Binary
install -Dm0755 target/release/punktfunk-host %{buildroot}%{_bindir}/punktfunk-host
# The PyroWave encode worker — a SEPARATE executable in the same bindir (the host resolves it as a
# sibling of /proc/self/exe). This is the ONLY binary in this package that carries a capability;
# see the %%caps note in %%files.
install -Dm0755 target/release/punktfunk-encode-worker %{buildroot}%{_bindir}/punktfunk-encode-worker

# udev rule — /dev/uinput access for virtual gamepads (input group).
install -Dm0644 scripts/60-punktfunk.rules %{buildroot}%{_udevrulesdir}/60-punktfunk.rules

# WirePlumber policy — hold a DualSense's sound card open (GE-Proton's raw-open self-race) and
# keep it from driving the graph clock. See the file's own comments.
install -Dm0644 scripts/60-punktfunk-dualsense.conf %{buildroot}%{_datadir}/wireplumber/wireplumber.conf.d/60-punktfunk-dualsense.conf

# ALSA UCM for the DualSense's own sound card — the `SpeakerHaptic` device `alsa-ucm-conf` has
# never carried. Without it the card's only playback route is a 1-channel `Speaker` split,
# GE-Proton mints its "Sony controller speaker" endpoint from that lone mono sink, and games
# that open it overrun it (a reliable EXCEPTION_ACCESS_VIOLATION in Spider-Man Remastered).
# Nothing here replaces an `alsa-ucm-conf` file: the vid:pid drop-ins under conf.d/ only
# redefine which profile the DualSense resolves to. Complements the WirePlumber policy above
# rather than overlapping it — that one governs how the card's nodes BEHAVE, this one governs
# which nodes exist. See scripts/alsa-ucm2/ for the mechanism.
install -Dm0644 scripts/alsa-ucm2/USB-Audio/conf.d/054c-0ce6.conf \
  %{buildroot}%{_datadir}/alsa/ucm2/USB-Audio/conf.d/054c-0ce6.conf
install -Dm0644 scripts/alsa-ucm2/USB-Audio/conf.d/054c-0df2.conf \
  %{buildroot}%{_datadir}/alsa/ucm2/USB-Audio/conf.d/054c-0df2.conf
install -Dm0644 scripts/alsa-ucm2/USB-Audio/Punktfunk/DualSense-PS5-Haptic.conf \
  %{buildroot}%{_datadir}/alsa/ucm2/USB-Audio/Punktfunk/DualSense-PS5-Haptic.conf
install -Dm0644 scripts/alsa-ucm2/USB-Audio/Punktfunk/DualSense-PS5-Haptic-HiFi.conf \
  %{buildroot}%{_datadir}/alsa/ucm2/USB-Audio/Punktfunk/DualSense-PS5-Haptic-HiFi.conf

# Managed gamescope takeover on DM-autologin boxes (Nobara's plasmalogin): a root helper + polkit
# action let the host stop/restore the display manager for the stream without a hand-installed
# polkit rule. The helper derives the DM unit itself — callers can't name arbitrary units.
install -Dm0755 scripts/pf-dm-helper %{buildroot}%{_libexecdir}/punktfunk/pf-dm-helper
install -Dm0644 scripts/io.unom.punktfunk.dm-helper.policy %{buildroot}%{_datadir}/polkit-1/actions/io.unom.punktfunk.dm-helper.policy
# ...and the other half of stopping a display manager: with it stopped the box has no active local
# session, so logind's own power actions fall to auth_admin_keep and Steam's power menu goes quiet
# mid-stream. Scoped to the same (shipped-empty) punktfunk group the helper above gates on.
install -Dm0644 packaging/linux/49-punktfunk-power.rules %{buildroot}%{_datadir}/polkit-1/rules.d/49-punktfunk-power.rules

# vhci-hcd autoload — the usbip transport that makes the virtual Steam Deck controller a
# real USB device (Steam Input only adopts those; the UHID fallback is invisible to Steam).
install -Dm0644 scripts/punktfunk-modules.conf %{buildroot}%{_prefix}/lib/modules-load.d/punktfunk.conf

# UDP socket-buffer tuning (32 MB) — without it the kernel clamps the host's SO_SNDBUF to ~416 KB
# and high-bitrate frames overflow it (send-side loss). systemd-sysctl applies it at boot.
install -Dm0644 scripts/99-punktfunk-net.conf %{buildroot}%{_prefix}/lib/sysctl.d/99-punktfunk-net.conf

# Web-console-triggered updates (host-update-from-web-console.md §7): the dep-free root
# helper + its oneshot system unit + the polkit rule scoping it to the (shipped-empty)
# punktfunk-update group. Also rides into the Bazzite sysext image via rpm2cpio.
install -Dm0755 target/release/pf-update %{buildroot}%{_libexecdir}/punktfunk/pf-update
install -Dm0644 packaging/linux/punktfunk-update.service %{buildroot}%{_unitdir}/punktfunk-update.service
install -Dm0644 packaging/linux/49-punktfunk-update.rules %{buildroot}%{_datadir}/polkit-1/rules.d/49-punktfunk-update.rules
install -Dm0755 packaging/linux/restart-user-units.sh %{buildroot}%{_libexecdir}/punktfunk/restart-user-units

# systemd *user* unit (the host runs in the graphical session, not as root).
install -Dm0644 scripts/punktfunk-host.service %{buildroot}%{_userunitdir}/punktfunk-host.service
# The source unit's ExecStart points at the dev source tree; a packaged install has the binary at
# %{_bindir}. Rewrite it so a fresh install (no hand-rolled unit) starts the installed binary.
sed -i 's#%h/punktfunk/target/release/punktfunk-host#%{_bindir}/punktfunk-host#' %{buildroot}%{_userunitdir}/punktfunk-host.service
# Optional drop-in for a DESKTOP-LOGIN host: binds the host to graphical-session.target so a
# Plasma/GNOME restart restarts it instead of leaving it on a dead compositor connection. Shipped
# under %{_datadir}/%{name} (NOT as an active drop-in) because it is wrong for the appliance route —
# the operator copies it into ~/.config/systemd/user/punktfunk-host.service.d/ when they want it.
install -Dm0644 scripts/punktfunk-host-desktop-session.conf %{buildroot}%{_datadir}/%{name}/punktfunk-host-desktop-session.conf

# Install-kind + channel marker, read by the host's update-check surface (planning:
# host-update-from-web-console.md §4.1). `pf_channel` is defined by build-rpm.sh (canary
# when the release override starts `0.ci`); a plain local rpmbuild is stable.
printf 'dnf %{?pf_channel}%{!?pf_channel:stable}\n' > %{buildroot}%{_datadir}/%{name}/install-kind

# Optional headless KDE session unit (the kwin streaming appliance): brings up `kwin --virtual` on
# wayland-kde via the packaged run-headless-kde.sh, so the host's kwin backend has a session whose
# privileged screencast protocol it can bind. Repoint its ExecStart from the dev source tree to the
# installed script. NOT enabled by default — only kwin-backend hosts (e.g. Fedora/Ubuntu KDE) need it.
install -Dm0644 scripts/punktfunk-kde-session.service %{buildroot}%{_userunitdir}/punktfunk-kde-session.service
sed -i 's#%h/punktfunk/scripts/headless/run-headless-kde.sh#%{_datadir}/%{name}/headless/run-headless-kde.sh#' %{buildroot}%{_userunitdir}/punktfunk-kde-session.service

# KWin authorization for Desktop-mode (KWin) streaming: a non-launcher .desktop whose
# X-KDE-Wayland-Interfaces grants the host the restricted zkde_screencast (virtual output) +
# fake_input globals on an interactive Plasma session. Must ship with the host so it is present
# before the host first connects (KWin caches the per-exe grant). Replaces the old manual
# KWIN_WAYLAND_NO_PERMISSION_CHECKS hack for the screencast permission.
install -Dm0644 packaging/linux/io.unom.Punktfunk.Host.desktop \
                %{buildroot}%{_datadir}/applications/io.unom.Punktfunk.Host.desktop

# Scheduling headroom for the host's data-plane threads (see the no-caps note in %%files): raise
# the user-session nice hard limit so pf-frame's setpriority() also works where RealtimeKit isn't
# running. A limit, not a grant — takes effect at the user's next login.
install -Dm0644 packaging/linux/50-punktfunk-nice.conf \
                %{buildroot}%{_unitdir}/user@.service.d/50-punktfunk-nice.conf

# Status tray: the per-user SNI icon + its XDG autostart entry (self-gating: --autostart exits
# silently for users who don't run a host) + the hicolor status icons it names.
install -Dm0755 target/release/punktfunk-tray %{buildroot}%{_bindir}/punktfunk-tray
install -Dm0644 packaging/linux/io.unom.Punktfunk.Tray.desktop \
                %{buildroot}%{_sysconfdir}/xdg/autostart/io.unom.Punktfunk.Tray.desktop
for sz in 22x22 48x48; do
  for png in packaging/linux/icons/hicolor/$sz/apps/*.png; do
    install -Dm0644 "$png" %{buildroot}%{_datadir}/icons/hicolor/$sz/apps/"$(basename "$png")"
  done
done
%endif

# --- client subpackage ---
install -Dm0755 target/release/punktfunk-client %{buildroot}%{_bindir}/punktfunk-client
# The session streamer the shell execs for a connect (resolved as its sibling in %{_bindir}).
install -Dm0755 target/release/punktfunk-session %{buildroot}%{_bindir}/punktfunk-session
# The headless CLI (design/client-architecture-split.md §4).
install -Dm0755 target/release/punktfunk %{buildroot}%{_bindir}/punktfunk
install -Dm0644 packaging/linux/io.unom.Punktfunk.desktop \
                %{buildroot}%{_datadir}/applications/io.unom.Punktfunk.desktop
# Second launcher, straight into the gamepad console (`--browse`) — the couch entry point.
install -Dm0644 packaging/linux/io.unom.Punktfunk.Console.desktop \
                %{buildroot}%{_datadir}/applications/io.unom.Punktfunk.Console.desktop
# The app icon the desktop entry (and the About dialog) name. Without it the launcher falls
# back to a generic monitor glyph, which is what shipped until now.
install -Dm0644 packaging/linux/icons/hicolor/scalable/apps/io.unom.Punktfunk.svg \
                %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/io.unom.Punktfunk.svg
# DualSense hidraw access (full pad fidelity through SDL's HIDAPI driver).
install -Dm0644 scripts/70-punktfunk-client.rules \
                %{buildroot}%{_udevrulesdir}/70-punktfunk-client.rules
# UDP receive-buffer tuning (32 MB) — the client asks for a 32 MB SO_RCVBUF; without raising
# net.core.rmem_max the kernel clamps it and high-bitrate streams overflow at the receiver
# (measured: 4 MB cap = 31.6% loss at 2 Gbps, 32 MB = 0%). Distinct filename from the host's so
# both can be installed on one box.
install -Dm0644 scripts/99-punktfunk-client-net.conf \
                %{buildroot}%{_prefix}/lib/sysctl.d/99-punktfunk-client-net.conf

# One-tap client updates (`punktfunk-client --apply-update`, which is what the Decky plugin
# runs): the same root helper the host subpackage ships, under the CLIENT's own paths. Separate
# paths are not tidiness — rpm refuses two subpackages owning one file, and a client-only box
# (a Deck, an aarch64 build with %%{without host}) must be able to install this on its own.
install -Dm0755 target/release/pf-update %{buildroot}%{_libexecdir}/punktfunk/pf-update-client
install -Dm0644 packaging/linux/punktfunk-client-update.service \
                %{buildroot}%{_unitdir}/punktfunk-client-update.service
sed -i 's#%{_libexecdir}/punktfunk/pf-update#%{_libexecdir}/punktfunk/pf-update-client#' \
       %{buildroot}%{_unitdir}/punktfunk-client-update.service
install -Dm0644 packaging/linux/49-punktfunk-client-update.rules \
                %{buildroot}%{_datadir}/polkit-1/rules.d/49-punktfunk-client-update.rules
# Install-kind + channel marker for the CLIENT, read by `punktfunk-client --check-update`. Its
# own DIRECTORY, not just its own filename: the host subpackage claims %{_datadir}/%{name}/*
# with a glob, so a sibling file there would be owned by both and `dnf install punktfunk
# punktfunk-client` would fail on the conflict.
install -d %{buildroot}%{_datadir}/punktfunk-client
printf 'dnf %{?pf_channel}%{!?pf_channel:stable}\n' > %{buildroot}%{_datadir}/punktfunk-client/install-kind

%if %{with host}
# Headless session helpers + example config + OpenAPI doc (reference material).
install -d %{buildroot}%{_datadir}/%{name}/headless
install -Dm0755 scripts/headless/run-headless-kde.sh   %{buildroot}%{_datadir}/%{name}/headless/run-headless-kde.sh
install -Dm0755 scripts/headless/run-headless-sway.sh  %{buildroot}%{_datadir}/%{name}/headless/run-headless-sway.sh
# RemoteDesktop grant pre-seed for headless libei input (run-headless-kde.sh copies it in).
install -Dm0644 scripts/headless/kde-authorized        %{buildroot}%{_datadir}/%{name}/headless/kde-authorized
# Virtual "Punktfunk" speaker (null sink the host captures/streams; run-headless-kde.sh installs it).
install -Dm0644 scripts/headless/punktfunk-sink.conf   %{buildroot}%{_datadir}/%{name}/headless/punktfunk-sink.conf
install -Dm0644 scripts/host.env.example               %{buildroot}%{_datadir}/%{name}/host.env.example
install -Dm0644 packaging/bazzite/host.env             %{buildroot}%{_datadir}/%{name}/host.env.bazzite
install -Dm0644 packaging/kde/host.env                 %{buildroot}%{_datadir}/%{name}/host.env.kde
# Bazzite KDE Desktop-mode one-shot setup (seeds the RemoteDesktop grant for libei input; the
# screencast/virtual-output grant ships as io.unom.Punktfunk.Host.desktop, installed above).
install -d %{buildroot}%{_datadir}/%{name}/bazzite
install -Dm0755 packaging/bazzite/kde-desktop-setup.sh %{buildroot}%{_datadir}/%{name}/bazzite/kde-desktop-setup.sh
# SELinux dontaudit drop-in for Bazzite/SteamOS: Valve's ds_inhibit (steamos-manager) walks
# /proc/*/fd on every open/close of a hid-playstation hidraw — our virtual DualSense — and the
# denied walk sprays ~324 AVCs/sec, which setroubleshootd amplifies into a box-wide stall that
# starves the stream. Shipped as CIL source (the policy STORE is host state); inserted by %%post
# below / punktfunk-sysext post_merge where steamos-manager exists. See the file's header.
install -Dm0644 packaging/bazzite/punktfunk-ds-inhibit.cil \
                %{buildroot}%{_datadir}/%{name}/selinux/punktfunk-ds-inhibit.cil
# Layered-update helper for rpm-ostree hosts: `rpm-ostree upgrade` only re-resolves layered
# packages when the BASE changes, so a frozen Bazzite base pins punktfunk forever. The script
# forces a re-resolve of just this layer (--uninstall + --install of the same names in one
# transaction). It is exactly the command pf-update-check hands an rpm-ostree host
# (`sudo /usr/share/punktfunk/update-punktfunk.sh`, crates/pf-update-check/src/detect.rs), so it
# has to exist at that path — an ostree box has no repo checkout to run it from. It only shells
# out to rpm-ostree/rpm/systemctl, so the installed copy is self-contained. Top level, not
# bazzite/, because the hint (and any Fedora-Atomic host) names that path.
install -Dm0755 packaging/bazzite/update-punktfunk.sh %{buildroot}%{_datadir}/%{name}/update-punktfunk.sh
# Headless GAME-mode fix: a gamescope-session-plus sessions.d drop-in that falls back to gamescope's
# headless backend when no display is connected (so "Switch to Game Mode" works on a display-less
# streaming host instead of crashing + 5-striking back to desktop). No-op on display-attached boxes.
# Sourced by gamescope-session-plus as /etc/gamescope-session-plus/sessions.d/steam (after its
# /usr/share defaults). Harmless on non-gamescope systems (the file is simply never read).
install -Dm0644 packaging/bazzite/gamescope-headless-session \
                %{buildroot}/etc/gamescope-session-plus/sessions.d/steam
install -Dm0644 api/openapi.json                  %{buildroot}%{_datadir}/%{name}/openapi.json
# firewalld service definitions (shared across all Linux packaging). Fedora/RHEL enable firewalld by
# default, so these matter here; NOT auto-enabled — %post prints the enable command. Owned by the
# firewalld package's dir; we drop only the files (same pattern as the sysctl.d file above).
install -Dm0644 packaging/linux/punktfunk-gamestream.xml \
                %{buildroot}%{_prefix}/lib/firewalld/services/punktfunk-gamestream.xml
install -Dm0644 packaging/linux/punktfunk-native.xml \
                %{buildroot}%{_prefix}/lib/firewalld/services/punktfunk-native.xml
# Web console opener (TCP 47992) — only meaningful with the web subpackage, opened deliberately.
install -Dm0644 packaging/linux/punktfunk-web.xml \
                %{buildroot}%{_prefix}/lib/firewalld/services/punktfunk-web.xml
%endif

%if %{with web}
# --- web console subpackage (punktfunk-web) ---
install -d %{buildroot}%{_datadir}/punktfunk-web/.output
cp -r web/.output/server %{buildroot}%{_datadir}/punktfunk-web/.output/server
cp -r web/.output/public %{buildroot}%{_datadir}/punktfunk-web/.output/public
# PATH-stable launcher (matches the .deb's /usr/bin/punktfunk-web-server) — runs on punktfunk-bun.
cat > %{buildroot}%{_bindir}/punktfunk-web-server <<'WRAP'
#!/bin/sh
exec /usr/libexec/punktfunk-bun/bun /usr/share/punktfunk-web/.output/server/index.mjs "$@"
WRAP
chmod 0755 %{buildroot}%{_bindir}/punktfunk-web-server
# systemd --user units: the console runs per-user; web-init generates the login password.
install -Dm0644 scripts/punktfunk-web.service      %{buildroot}%{_userunitdir}/punktfunk-web.service
install -Dm0644 scripts/punktfunk-web-init.service %{buildroot}%{_userunitdir}/punktfunk-web-init.service
install -Dm0755 scripts/web-init.sh                %{buildroot}%{_datadir}/punktfunk-web/web-init.sh
install -Dm0644 web/web.env.example                %{buildroot}%{_datadir}/punktfunk-web/web.env.example
%endif

%if %{with scripting}
# --- plugin/script runner subpackage (punktfunk-scripting) ---
install -Dm0644 runner-cli.js %{buildroot}%{_datadir}/punktfunk-scripting/runner-cli.js
# PATH-stable launcher (matches the .deb's) — runs the bundle on punktfunk-bun.
cat > %{buildroot}%{_bindir}/punktfunk-scripting <<'WRAP'
#!/bin/sh
exec /usr/libexec/punktfunk-bun/bun /usr/share/punktfunk-scripting/runner-cli.js "$@"
WRAP
chmod 0755 %{buildroot}%{_bindir}/punktfunk-scripting
# systemd --user unit — installed but NOT auto-enabled (opt-in; the runner is inert until you add
# scripts/plugins). Enable with `systemctl --user enable --now punktfunk-scripting`.
install -Dm0644 scripts/punktfunk-scripting.service %{buildroot}%{_userunitdir}/punktfunk-scripting.service
%endif

%if %{with web} || %{with scripting}
# --- vendored bun runtime (punktfunk-bun), the build env's bun: the CI rpm image's pin ---
install -Dm0755 "$(command -v bun)" %{buildroot}%{_libexecdir}/punktfunk-bun/bun
%endif

%if %{with host}
%files
%license LICENSE-MIT LICENSE-APACHE THIRD-PARTY-NOTICES.txt
%doc README.md packaging/README.md
# NO %caps() on the host binary. 0.26.0-1 declared `%caps(cap_sys_nice=ep)` here for the
# GPU-priority lever and that BROKE DESKTOP STREAMING ON EVERY KDE BOX — on Fedora and, via
# rpm-ostree layering, on Bazzite, where it was field-reported as
# "KWin does not expose zkde_screencast_unstable_v1 to this client".
#
# KWin hands out its restricted Wayland protocols (zkde_screencast_unstable_v1 for the virtual
# output, org_kde_kwin_fake_input for input) only to a client it can IDENTIFY, by resolving that
# client's /proc/<pid>/exe and matching it against an installed .desktop's Exec= — ours is the
# io.unom.Punktfunk.Host.desktop installed below. The kernel refuses that readlink to any reader
# whose effective set is not a superset of the target's PERMITTED set (cap_ptrace_access_check),
# and KWin holds no capabilities. So a capability here makes the host unidentifiable: KWin's
# executablePath() is empty, no .desktop can match, and the globals are never advertised.
# Measured on kernel 7.1.6 — see packaging/arch/punktfunk-host.install for the full matrix, incl.
# why neither prctl(PR_SET_DUMPABLE, 1) nor systemd AmbientCapabilities= rescues it.
#
# The cost of not having it is pacing only: pf-zerocopy walks REALTIME -> HIGH -> default when a
# priority class is refused, and pf-frame's thread nice falls back to RealtimeKit (the same
# unprivileged broker PipeWire clients use — no capability enters the permitted set, so the KWin
# identification above is untouched) and to the user@.service.d LimitNICE drop-in shipped below.
# Only on a box with neither does it remain the best-effort no-op 0.25.0 shipped with.
#
# rpm applies file capabilities from package metadata, so a package built WITHOUT %caps() installs
# the binary with none and an upgrade from 0.26.0-1 clears it — no scriptlet needed.
%{_bindir}/punktfunk-host
# CAP_SYS_NICE on the ENCODE WORKER — the grant 0.26.0-1 aimed at the wrong binary, on a binary
# that can carry it. punktfunk-encode-worker is a separate executable (never a hardlink or a host
# subcommand: a shared inode would share the capability and re-create the breakage above). It is
# spawned per PyroWave session, speaks one socketpair to its parent and never touches Wayland,
# D-Bus or the network — so nothing ever resolves ITS /proc/<pid>/exe and the KWin identification
# path stays clear.
#
# Declared with %%caps rather than a %%post setcap because that is the rpm-native form: rpm applies
# the capability at install, RESTORES it on upgrade (a replaced file is a new inode), and verifies
# it under `rpm -V`. A scriptlet does none of those. This also covers Bazzite via rpm-ostree
# layering, which honours file capabilities from package metadata.
#
# Why: PyroWave encodes on the GPU shader cores the game saturates, and an elevated
# VK_KHR_global_priority queue is the preemption lever. Every driver tested (NVIDIA and RADV)
# refuses EVERY class without CAP_SYS_NICE. Measured on .21 (RTX 5070 Ti): encode p99 6.4 -> 4.4 ms.
# Narrow — scheduling priority only, no filesystem/network/user-switching privilege, not setuid.
# Best-effort by construction: an uncapped worker still encodes, at default priority.
#
# Debugging the WORKER (not the host): a capability makes it AT_SECURE, so the loader ignores
# LD_LIBRARY_PATH/LD_PRELOAD for it and core dumps are suppressed by default.
%caps(cap_sys_nice=ep) %{_bindir}/punktfunk-encode-worker
%dir %{_unitdir}/user@.service.d
%{_unitdir}/user@.service.d/50-punktfunk-nice.conf
%{_bindir}/punktfunk-tray
%{_udevrulesdir}/60-punktfunk.rules
%{_datadir}/wireplumber/wireplumber.conf.d/60-punktfunk-dualsense.conf
# The DualSense UCM drop-in. Both directories are ours: alsa-ucm-conf ships neither
# USB-Audio/conf.d nor USB-Audio/Punktfunk, so owning them collides with nothing.
%dir %{_datadir}/alsa/ucm2/USB-Audio/conf.d
%dir %{_datadir}/alsa/ucm2/USB-Audio/Punktfunk
%{_datadir}/alsa/ucm2/USB-Audio/conf.d/054c-0ce6.conf
%{_datadir}/alsa/ucm2/USB-Audio/conf.d/054c-0df2.conf
%{_datadir}/alsa/ucm2/USB-Audio/Punktfunk/DualSense-PS5-Haptic.conf
%{_datadir}/alsa/ucm2/USB-Audio/Punktfunk/DualSense-PS5-Haptic-HiFi.conf
%dir %{_libexecdir}/punktfunk
%{_libexecdir}/punktfunk/pf-dm-helper
%{_libexecdir}/punktfunk/pf-update
%{_libexecdir}/punktfunk/restart-user-units
%{_unitdir}/punktfunk-update.service
%{_datadir}/polkit-1/rules.d/49-punktfunk-update.rules
%{_datadir}/polkit-1/rules.d/49-punktfunk-power.rules
%{_datadir}/polkit-1/actions/io.unom.punktfunk.dm-helper.policy
%{_prefix}/lib/modules-load.d/punktfunk.conf
%{_prefix}/lib/sysctl.d/99-punktfunk-net.conf
%{_prefix}/lib/firewalld/services/punktfunk-gamestream.xml
%{_prefix}/lib/firewalld/services/punktfunk-native.xml
%{_prefix}/lib/firewalld/services/punktfunk-web.xml
%{_userunitdir}/punktfunk-host.service
%{_userunitdir}/punktfunk-kde-session.service
%{_datadir}/applications/io.unom.Punktfunk.Host.desktop
%{_sysconfdir}/xdg/autostart/io.unom.Punktfunk.Tray.desktop
%{_datadir}/icons/hicolor/*/apps/punktfunk-tray*.png
%dir /etc/gamescope-session-plus
%dir /etc/gamescope-session-plus/sessions.d
%config(noreplace) /etc/gamescope-session-plus/sessions.d/steam
%dir %{_datadir}/%{name}
%{_datadir}/%{name}/*
%endif

%files client
# The CLIENT-scoped notices, not the workspace-wide root file: the root one covers the host's
# dependency graph, which is a superset of this subpackage's. Same file the GTK shell shows on
# About → Legal (scripts/gen-third-party-notices.sh
# generates both). `%%license` installs it under its basename, so the path stays the usual one.
%license LICENSE-MIT LICENSE-APACHE clients/linux/THIRD-PARTY-NOTICES.txt
%{_bindir}/punktfunk-client
%{_bindir}/punktfunk-session
%{_bindir}/punktfunk
%{_datadir}/applications/io.unom.Punktfunk.desktop
%{_datadir}/applications/io.unom.Punktfunk.Console.desktop
%{_datadir}/icons/hicolor/scalable/apps/io.unom.Punktfunk.svg
%{_udevrulesdir}/70-punktfunk-client.rules
%{_prefix}/lib/sysctl.d/99-punktfunk-client-net.conf
# Co-owned with the host subpackage (rpm allows that for DIRECTORIES, unlike files) so a
# client-only install — the aarch64 `%%{without host}` build — still owns the dir it created.
%dir %{_libexecdir}/punktfunk
%{_libexecdir}/punktfunk/pf-update-client
%{_unitdir}/punktfunk-client-update.service
%{_datadir}/polkit-1/rules.d/49-punktfunk-client-update.rules
%dir %{_datadir}/punktfunk-client
%{_datadir}/punktfunk-client/install-kind

%if %{with web}
%files web
%license LICENSE-MIT LICENSE-APACHE THIRD-PARTY-NOTICES.txt
%{_bindir}/punktfunk-web-server
%dir %{_datadir}/punktfunk-web
%{_datadir}/punktfunk-web/.output
%{_datadir}/punktfunk-web/web-init.sh
%{_datadir}/punktfunk-web/web.env.example
%{_userunitdir}/punktfunk-web.service
%{_userunitdir}/punktfunk-web-init.service
%endif

%if %{with scripting}
%files scripting
%license LICENSE-MIT LICENSE-APACHE THIRD-PARTY-NOTICES.txt
%{_bindir}/punktfunk-scripting
%dir %{_datadir}/punktfunk-scripting
%{_datadir}/punktfunk-scripting/runner-cli.js
%{_userunitdir}/punktfunk-scripting.service
%endif

%if %{with web} || %{with scripting}
%files bun
%dir %{_libexecdir}/punktfunk-bun
%{_libexecdir}/punktfunk-bun/bun
%endif

%post client
# The (empty) opt-in group for one-tap client updates — nobody is auto-added. Also created by
# the host subpackage's %%post; groupadd is idempotent, so whichever lands first wins and the
# other is a no-op.
getent group punktfunk-update >/dev/null 2>&1 || groupadd --system punktfunk-update 2>/dev/null || :
# Pick up the DualSense hidraw rule without a reboot (best-effort; on rpm-ostree it
# applies on the next boot into the layered deployment).
udevadm control --reload-rules 2>/dev/null || :
udevadm trigger --subsystem-match=hidraw 2>/dev/null || :
# Apply the UDP recv-buffer tuning now (also auto-applied at boot by systemd-sysctl; on
# rpm-ostree it takes effect on the next boot into the layered deployment).
sysctl -p %{_prefix}/lib/sysctl.d/99-punktfunk-client-net.conf >/dev/null 2>&1 || :
# Register the punktfunk:// scheme handler the .desktop entry declares (deb and arch do the
# same in their own scriptlets) — without this, xdg-open and browser prompts have no idea the
# client claims those links.
update-desktop-database %{_datadir}/applications >/dev/null 2>&1 || :

%if %{with host}
%post
# The (empty) opt-in group for web-console-triggered updates — nobody is auto-added.
getent group punktfunk-update >/dev/null 2>&1 || groupadd --system punktfunk-update 2>/dev/null || :
# Owns the usbip vhci attach/detach nodes (60-punktfunk.rules). Deliberately NOT 'input': writing
# 'attach' materialises an arbitrary emulated USB device — a root-only kernel primitive that must
# not ride on the group users are told to join for gamepads (security-review 2026-08-05 M-4).
# It is ALSO the group `pf-dm-helper` authorizes on (the polkit action must stay `allow_any`, so
# membership is the real gate) — so it is what a managed gamescope takeover needs to stop the
# display manager. Creating it is necessary and NOT sufficient for either use: membership is.
getent group punktfunk >/dev/null 2>&1 || groupadd --system punktfunk 2>/dev/null || :
# Reload udev so /dev/uinput picks up the new rule without a reboot (best-effort).
udevadm control --reload-rules 2>/dev/null || :
udevadm trigger --subsystem-match=misc 2>/dev/null || :
# Apply the UDP socket-buffer tuning (also auto-applied at boot by systemd-sysctl; on rpm-ostree
# it takes effect on the next boot into the layered deployment).
sysctl -p %{_prefix}/lib/sysctl.d/99-punktfunk-net.conf >/dev/null 2>&1 || :
# Bazzite/SteamOS only (keyed on the steamos-manager binary): insert the ds_inhibit dontaudit
# drop-in — Valve's ds_inhibit walks /proc on every open/close of our virtual DualSense's hidraw,
# the denied walk sprays AVCs, and setroubleshootd amplifies that into a box-wide stall (see
# packaging/bazzite/punktfunk-ds-inhibit.cil). Keyed on the module NAME for idempotence (a policy
# rebuild costs seconds — rename the file if the rules ever change). Best-effort and never fatal:
# rpm-ostree's scriptlet sandbox may refuse semodule; the sysext post_merge and the README's
# manual command cover that path.
if command -v semodule >/dev/null 2>&1 && [ -e /usr/lib/steamos-manager ] &&
   ! semodule -l 2>/dev/null | grep -qx punktfunk-ds-inhibit; then
    semodule -i %{_datadir}/%{name}/selinux/punktfunk-ds-inhibit.cil >/dev/null 2>&1 || :
fi
echo "punktfunk installed. Add yourself to the 'input' group (sudo usermod -aG input \$USER)"
# Naming only the usbip pad here is how a Nobara host shipped broken: its owner had no Deck pad, so
# they correctly skipped this group — and then every managed gamescope takeover degraded silently,
# because pf-dm-helper (which stops the display manager for the stream) gates on THIS membership.
echo "ALSO join 'punktfunk' if this box streams Steam Gaming Mode (gamescope) or you want the"
echo "virtual Steam Deck pad: sudo usermod -aG punktfunk \$USER   # then log out and back in"
echo "  — it authorizes stopping the display manager for a managed gamescope session, and the"
echo "    pad's usbip nodes; it can emulate arbitrary USB devices, so join it only on a box you trust."
echo "then enable the host: systemctl --user enable --now punktfunk-host"
echo "Settings: the console (Host -> Settings). A line in ~/.config/punktfunk/host.env locks that setting there;"
echo "  annotated templates: %{_datadir}/%{name}/host.env.{bazzite,kde,example}"
# Fedora/RHEL run firewalld by default — point the way to the installed service definitions.
if command -v firewall-cmd >/dev/null 2>&1; then
    echo "Firewall (firewalld): sudo firewall-cmd --reload &&"
    echo "    sudo firewall-cmd --permanent --add-service=punktfunk-gamestream && sudo firewall-cmd --reload"
    echo "    (use punktfunk-native for the native-only host)"
fi
# A running firewalld serves the service it last loaded, so a port this upgrade added to the XML is
# closed until a reload. `--info-service` asks the daemon, i.e. reads that stale copy.
if command -v firewall-cmd >/dev/null 2>&1 &&
   firewall-cmd --state >/dev/null 2>&1 &&
   firewall-cmd --query-service=punktfunk-web >/dev/null 2>&1 &&
   ! firewall-cmd --info-service=punktfunk-web 2>/dev/null | grep -q '47993'; then
    echo ""
    echo "punktfunk: the punktfunk-web firewalld service now also covers TCP 47993 (plugin UIs)."
    echo "  Plugin interfaces will not load in the console until:  sudo firewall-cmd --reload"
fi
if command -v firewall-cmd >/dev/null 2>&1 &&
   firewall-cmd --state >/dev/null 2>&1 &&
   firewall-cmd --query-service=punktfunk-native >/dev/null 2>&1 &&
   ! firewall-cmd --info-service=punktfunk-native 2>/dev/null | grep -q '9778'; then
    echo ""
    echo "punktfunk: the punktfunk-native firewalld service now also covers UDP 9778 (browser"
    echo "  streaming). A browser cannot connect to this host until:  sudo firewall-cmd --reload"
fi
# Conflicting Moonlight-compatible host (Sunshine/Apollo/...): reuse the host's own detector so the
# warning stays in one place. Exit 1 = something found; never fail the install on it.
if command -v punktfunk-host >/dev/null 2>&1; then
    if ! conflict="$(punktfunk-host detect-conflicts 2>/dev/null)"; then
        echo ""
        echo "$conflict"
    fi
fi

# Any punktfunk server package update restarts the running services, once per transaction.
%transfiletriggerin -- %{_bindir}/punktfunk-host %{_datadir}/punktfunk-web %{_datadir}/punktfunk-scripting %{_libexecdir}/punktfunk-bun
%{_libexecdir}/punktfunk/restart-user-units
%endif

%if %{with web}
%post web
echo "punktfunk-web installed. Enable the console for your user:"
echo "    systemctl --user enable --now punktfunk-web"
echo "A login password is generated on first start. Read it once, before you sign in:"
# From the 0600 file, NOT the journal: the journal is persistent and group-readable (adm /
# systemd-journal on Debian-family), so telling people to fish a password out of it published the
# secret to every member of those groups. The read works until the console hashes the line.
echo "    sed -n 's/^PUNKTFUNK_UI_PASSWORD=//p' \${XDG_CONFIG_HOME:-\$HOME/.config}/punktfunk/web-password"
echo "After that the console keeps only a salted hash, so a forgotten password is reset: put a"
echo "PUNKTFUNK_UI_PASSWORD=<your-password> line in that file and restart punktfunk-web."
echo "Then open https://<host-ip>:47992"
%endif

%if %{with scripting}
%post scripting
# `--global`, not `--user`: a scriptlet has no user session to act on, and this is the only
# mechanism that makes a `--user` unit on-by-default for everyone (it symlinks into
# /etc/systemd/user/…wants/). The game-library scanners are plugins now, so the runner is a default
# component rather than an add-on (design D9); it stays opt-OUT via
# `systemctl --user mask punktfunk-scripting`, since a plain `--user disable` cannot remove a global
# symlink. $1 == 1 is a first INSTALL — on an upgrade ($1 > 1) this must not undo an operator's mask.
if [ "$1" -eq 1 ] && command -v systemctl >/dev/null 2>&1; then
    systemctl --global enable punktfunk-scripting.service >/dev/null 2>&1 || :
fi
echo "punktfunk-scripting installed and enabled for all users."
echo "It runs your automation — game-library sources, scripts in"
echo "    ~/.config/punktfunk/scripts/  (loose .ts/.js files)"
echo "and plugins under ~/.config/punktfunk/plugins/."
echo "It starts with your next login; start it now with:"
echo "    systemctl --user start punktfunk-scripting"
echo "Don't want it? systemctl --user mask punktfunk-scripting"
%endif

%changelog
* Fri Jul 17 2026 punktfunk <packages@unom.io> - 0.0.1-3
- Add punktfunk-scripting subpackage (plugin/script runner, --with scripting; bun-bundled Effect SDK).
* Mon Jun 15 2026 punktfunk <packages@unom.io> - 0.0.1-2
- Add punktfunk-web subpackage (management console, --with web; auto-wired to the host token).
* Wed Jun 10 2026 punktfunk <packages@unom.io> - 0.0.1-1
- Initial RPM: punktfunk-host + udev rule + systemd user unit + headless helpers.
