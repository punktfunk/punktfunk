# Flatpak CI builder: everything flatpak.yml used to install per run, plus the Flathub
# runtime set the manifest declares. Content-keyed and rebuilt only when the ci/ tree
# changes (docker.yml `builders`).
#
#   docker build -f ci/flatpak-ci.Dockerfile -t punktfunk-flatpak-ci ci
#
# MEASURED on run 18855 (2026-08-17), a green 21m23s build, this image removes:
#   63 s  `dnf -y install nodejs`      (act_runner execs JS actions with the CONTAINER's node)
#  240 s  `Tooling` — 330 packages
#  168 s  restoring a 1.5 GB actions/cache of ~/.local/share/flatpak
# i.e. ~7.8 min of an 8 min preamble in front of a 6 min compile. The runtimes are the
# bulk of the image and the reason it exists; a package-only image would leave the
# biggest single step in place.
#
# WHY FEDORA and not the flathub org.flatpak.Builder image: flatpak.yml's job container
# must run bubblewrap under --privileged in the act_runner Docker executor, and Fedora
# ships a flatpak/flatpak-builder pair recent enough for the manifest with the kernel
# userns support already enabled. See flatpak.yml's header for the --privileged and
# --network host constraints, both of which still apply to this image.
FROM docker.io/library/fedora:43

# Distro packages come through ci-core's LAN mirror, at build time and in every job.
COPY pkg-mirror.sh /tmp/
RUN sh /tmp/pkg-mirror.sh && rm /tmp/pkg-mirror.sh

# nss-resolve trap, baked. fedora:43's nsswitch.conf is
# `hosts: files myhostname resolve [!UNAVAIL=return] dns`: `resolve` is nss-resolve
# (systemd-resolved), which does not run in a CI container. glibc consumers (git, curl,
# dnf) fall through to `dns`; flatpak/ostree's resolver does NOT — the absent-daemon
# socket connect trips `[!UNAVAIL=return]` and it reports "[6] Could not resolve
# hostname". flatpak.yml carries the same sed as a runtime step and had to repeat it
# after its dnf transaction, because a systemd upgrade's authselect trigger regenerates
# the file. Doing it here, after the last dnf in the image, ends that whack-a-mole for
# every consumer — the workflow's copy stays only as a guard for a lagging :latest.
RUN dnf -y install \
        # the build itself
        flatpak flatpak-builder \
        # ostree CLI: the seed step pulls the published channels' tip commits with it
        # (`dnf install flatpak` brings the LIBRARY, not the binary)
        ostree \
        # actions/checkout is a JS action and act_runner does not inject a node
        nodejs git git-lfs \
        # flatpak-cargo-generator.py needs aiohttp + tomlkit (NOT the old `toml`)
        python3 python3-aiohttp python3-tomlkit \
        # sign the OSTree repo + rsync it to unom-1; jq/curl for the registry publish
        gnupg2 rsync openssh-clients curl jq \
    && dnf clean all \
    && sed -i 's/resolve \[!UNAVAIL=return\] //' /etc/nsswitch.conf \
    && ! grep -q 'resolve \[!UNAVAIL=return\]' /etc/nsswitch.conf

# The Flathub runtime/SDK set, baked into the image's USER installation (/root/.local,
# HOME=/root in the job container) — the same path flatpak.yml used to restore from
# actions/cache, so `flatpak-builder --user` finds these with no further wiring.
#
# ⚠ KEEP IN SYNC with packaging/flatpak/io.unom.punktfunk.yml: GNOME_VERSION is the
# manifest's `runtime-version`, FREEDESKTOP_VERSION is what flatpak-builder resolves the
# two `sdk-extensions` to (it prints them as "Dependency Extension: … 25.08"). A drift
# here is not fatal — flatpak.yml's prefetch step downloads whatever is missing from
# Flathub, retried — but it silently costs ~1.5 GB and several minutes per run, which is
# precisely what this image exists to avoid. Bump both in the same commit as the manifest.
#
# `flatpak install` HERE, at docker-build time, needs no privileges — VERIFIED 2026-08-17
# in a plain `docker run` container where bwrap was proven broken first
# ("bwrap: No permissions to creating new namespace"): the install still exited 0 and
# `flatpak info --user` resolved the ref. flatpak's post-deploy triggers are the only
# part that wants bwrap and they are best-effort, so this layer does NOT need the
# --privileged that the CONSUMING job needs for flatpak-builder's actual sandbox.
#
# Related refs (org.freedesktop.Platform.GL.default{,-extra}, codecs-extra,
# org.gnome.Platform.Locale) come along by default and are deliberately kept: they are
# what `flatpak-builder --install-deps-only` would otherwise pull on every run, so a
# --no-related image would look smaller and cost more. org.gnome.Platform//50 alone
# unpacks to 2.4 GB; the whole set is the same content the 1.5 GB (compressed) runtime
# actions/cache already carried — this moves it from the cache server to the registry,
# where Docker keeps it on the runner's disk instead of re-extracting it every run.
ARG GNOME_VERSION=50
ARG FREEDESKTOP_VERSION=25.08
RUN flatpak remote-add --user --if-not-exists flathub \
        https://dl.flathub.org/repo/flathub.flatpakrepo \
    && flatpak install --user -y --noninteractive flathub \
        "org.gnome.Platform//${GNOME_VERSION}" \
        "org.gnome.Sdk//${GNOME_VERSION}" \
        "org.freedesktop.Sdk.Extension.rust-stable//${FREEDESKTOP_VERSION}" \
        "org.freedesktop.Sdk.Extension.llvm20//${FREEDESKTOP_VERSION}" \
    # Assert rather than trust. `flatpak install` treats its post-deploy triggers as
    # best-effort and this RUN's exit status would not notice a ref that failed to
    # deploy; an image that merely LOOKS warm would push the 1.5 GB back onto every
    # single run, where it reads as "flatpak is slow again" instead of a broken image.
    # Fail the image build loudly instead — docker.yml goes red and :latest never moves.
    && for ref in \
        "org.gnome.Platform//${GNOME_VERSION}" \
        "org.gnome.Sdk//${GNOME_VERSION}" \
        "org.freedesktop.Sdk.Extension.rust-stable//${FREEDESKTOP_VERSION}" \
        "org.freedesktop.Sdk.Extension.llvm20//${FREEDESKTOP_VERSION}" \
       ; do flatpak info --user "$ref" >/dev/null || exit 1; done \
    && flatpak list --user --columns=ref
