#!/usr/bin/env bash
# punktfunk-sysext — install/update the punktfunk host on Bazzite / Fedora Atomic as a
# systemd-sysext, the no-layering path (rpm-ostree layering is a last resort per the Bazzite
# docs: it slows every update and can block upgrades; a sysext never enters an rpm-ostree
# transaction, needs no reboot, and is trivially removable).
#
# The image overlays /usr from /var/lib/extensions/punktfunk.raw with the host, tray and web
# console + their udev/sysctl/systemd-user payload; the RPMs' /etc file (the gamescope
# session drop-in) rides inside at /usr/share/punktfunk/etc/ and is copied into the real /etc
# here (a sysext can only carry /usr).
#
# Bootstrap (the script also ships inside the image as /usr/bin/punktfunk-sysext):
#   curl -fsSLO https://git.unom.io/unom/punktfunk/raw/branch/main/packaging/bazzite/punktfunk-sysext.sh
#   sudo bash punktfunk-sysext.sh install            # or: install --channel canary
# Thereafter:
#   sudo punktfunk-sysext update | status | remove
#
# Feed: the Gitea generic package registry, one feed per Fedora major x channel
# (…/punktfunk-sysext/f43/, f43-canary, f44, …), each a SHA256SUMS + SHA256SUMS.asc + versioned
# .raw files — published by .gitea/workflows/rpm.yml from the same RPMs the (legacy) layering path
# uses. The image pins ID=fedora + VERSION_ID, so after a major OS rebase the old image is refused
# (not merged broken) and `punktfunk-sysext update` re-resolves against the new release.
#
# Trust: SHA256SUMS carries a detached OpenPGP signature (SHA256SUMS.asc) from packages@unom.io —
# the same key that signs our RPMs — and this script verifies it before believing a word of the
# manifest. The checksums alone could never have done that: they live on the same registry as the
# images they describe, so anything able to replace an image could replace its checksum too. The
# public key is baked in below rather than fetched, because a key fetched from the thing you are
# authenticating authenticates nothing.
#   A signature alone still only says "we signed these bytes, once" — not which feed they were
# signed for, nor whether they are the current ones. So the manifest also carries a `# FEED` and a
# `# SERIAL` header INSIDE the signed bytes, and fetch_manifest refuses one whose FEED is not the
# feed it fetched, or whose SERIAL is below the highest this box has accepted. Without both, anyone
# who can write the registry WITHOUT the key (a leaked write:package token) copies the canary
# manifest + images into the stable path, or puts last month's back, and every box verifies it
# happily. Same two rules the Rust updater enforces (crates/core/pf-update-check/src/manifest.rs).
set -euo pipefail

REGISTRY="${PUNKTFUNK_SYSEXT_REGISTRY:-https://git.unom.io/api/packages/unom/generic/punktfunk-sysext}"
CONF=/etc/punktfunk-sysext.conf
EXT_DIR=/var/lib/extensions
IMG="$EXT_DIR/punktfunk.raw"
SIDECAR="$EXT_DIR/.punktfunk.version"
FLOOR_FILE="$EXT_DIR/.punktfunk.serial-floor"
MARKER=/usr/lib/extension-release.d/extension-release.punktfunk
ETC_SRC=/usr/share/punktfunk/etc
PF_TMP="$(mktemp -d)"; trap 'rm -rf "$PF_TMP"' EXIT

# The feed's signing key: punktfunk packages <packages@unom.io>, AF245C506F4E4763. Identical to
# packaging/rpm/RPM-GPG-KEY-punktfunk — ONE key signs both the RPMs and this feed, so rotating it
# means updating both copies (the rotation runbook in packaging/rpm/README.md says so, and
# publish-sysext-feed.sh refuses to sign if the two ever disagree).
FEED_KEY='-----BEGIN PGP PUBLIC KEY BLOCK-----

mDMEai/2eRYJKwYBBAHaRw8BAQdAFxLGvh8wvzES9ylmxT4gy1i58EituotPyZwt
z+y9rbC0JXB1bmt0ZnVuayBwYWNrYWdlcyA8cGFja2FnZXNAdW5vbS5pbz6IkAQT
FgoAOBYhBDG6uOY81eoQ6beahK8kXFBvTkdjBQJqL/Z5AhsjBQsJCAcCBhUKCQgL
AgQWAgMBAh4BAheAAAoJEK8kXFBvTkdj1QsBAM0sI/qUzGEbuC2Zrk36QQBrUu/9
sy5uhYGZD6lMJ4uZAQC7W81H2gHlTDTA2Nq35HKW9IOU+Ll2c9fqa7fAIKf9Bg==
=e4Az
-----END PGP PUBLIC KEY BLOCK-----'

usage() {
  sed -n 's/^#\( \|$\)//p' "$0" | sed -n '1,20p'
  echo "usage: punktfunk-sysext install [--channel stable|canary] [--from-file X.raw]"
  echo "       punktfunk-sysext update [--from-file X.raw] | reapply | status | remove"
  echo "       reapply: re-run the host-state steps a sysext image cannot carry (groups, /etc"
  echo "                mirrors, udev, sysctl, modules) without reinstalling the image."
  exit "${1:-0}"
}
need_root() { [ "$(id -u)" = 0 ] || { echo "run as root (sudo)" >&2; exit 1; }; }

os_version_id() { . /etc/os-release; echo "${VERSION_ID%%.*}"; }
channel() { # shellcheck disable=SC1090
  [ -f "$CONF" ] && . "$CONF"; echo "${CHANNEL:-stable}"; }
# feed_name -> the feed this box reads: f43, f43-canary, … (Fedora major x channel). The publisher
# stamps this same name into the signed manifest, which is what makes the two comparable.
feed_name() {
  local suffix=""
  [ "$(channel)" = canary ] && suffix="-canary"
  echo "f$(os_version_id)$suffix"
}
feed_url() { echo "$REGISTRY/$(feed_name)"; }

# The highest manifest serial ever accepted for a feed — the anti-rollback floor, persisted the way
# the Rust updater persists its own per-channel `serial_floor` (crates/host/punktfunk-host/src/update.rs).
# Per FEED, never global: serials are publish timestamps, so a canary publish would otherwise raise
# the floor above the next stable manifest and lock the stable channel out.
serial_floor() {
  local v
  v="$(sed -n "s/^$(feed_name) //p" "$FLOOR_FILE" 2>/dev/null | head -n1)"
  case "$v" in ''|*[!0-9]*) echo 0 ;; *) echo "$v" ;; esac
}
# Raise it (never lower — the caller compares first). Best-effort and quiet on purpose: `status`
# runs unprivileged, and before the extensions dir exists at all, so it must report the feed rather
# than die — or nag — over not being able to record it. `2>/dev/null` comes BEFORE the redirect it
# is there to silence: redirections are set up left to right, so the other order still prints the
# shell's own "No such file or directory" to the terminal.
raise_serial_floor() {
  local f; f="$(feed_name)"
  { grep -v "^$f " "$FLOOR_FILE" 2>/dev/null || :; echo "$f $1"; } 2>/dev/null > "$FLOOR_FILE.new" \
    && mv -f "$FLOOR_FILE.new" "$FLOOR_FILE" 2>/dev/null || :
}

# verify_manifest SUMS SIG -> 0 iff SIG is a good detached signature over SUMS by FEED_KEY.
#   A throwaway keyring holding exactly our one key, so "good signature" and "signed by us" are the
#   same statement — any other signer comes back NO_PUBKEY, and gpg exits non-zero.
#   Spelled out with plain `if`s rather than `cond && action`: under `set -e` a failing test at the
#   end of an && list is a trap that only bites on the path nobody exercises (here: a corrupt
#   FEED_KEY), and this function must never abort the script — its whole job is to return a verdict.
verify_manifest() {
  local home rc=1
  home="$(mktemp -d)"; chmod 700 "$home"
  if printf '%s\n' "$FEED_KEY" | GNUPGHOME="$home" gpg --batch --quiet --import 2>/dev/null; then
    if GNUPGHOME="$home" gpg --batch --quiet --verify "$2" "$1" 2>/dev/null; then rc=0; fi
  fi
  rm -rf "$home"
  return "$rc"
}

# fetch_manifest -> download the feed's SHA256SUMS into $PF_TMP and verify its signature.
#   Returns non-zero (having said why) rather than exiting, so `status` can report a bad feed
#   instead of dying on it; install/update turn that into a hard stop.
fetch_manifest() {
  local feed sums sig want got serial floor
  feed="$(feed_url)"
  sums="$PF_TMP/SHA256SUMS"; sig="$PF_TMP/SHA256SUMS.asc"
  curl -fsSL -o "$sums" "$feed/SHA256SUMS" || { echo "cannot reach the feed $feed" >&2; return 1; }
  if [ "${PUNKTFUNK_SYSEXT_ALLOW_UNSIGNED:-0}" = 1 ]; then
    echo "!! PUNKTFUNK_SYSEXT_ALLOW_UNSIGNED=1 — the feed manifest is NOT being verified." >&2
    return 0
  fi
  # curl's own "404"/"could not open file" is noise here — a missing signature is an expected
  # state with a much better explanation below, so swallow it and say the useful thing instead.
  if ! curl -fsSL -o "$sig" "$feed/SHA256SUMS.asc" 2>/dev/null; then
    echo "!! the feed $feed has no SHA256SUMS.asc — refusing to install from an unsigned feed." >&2
    echo "!! (a feed published before signing existed; it is sealed on the next publish. To install" >&2
    echo "!!  from it anyway, knowing the images are unauthenticated: PUNKTFUNK_SYSEXT_ALLOW_UNSIGNED=1)" >&2
    return 1
  fi
  if ! command -v gpg >/dev/null 2>&1; then
    echo "!! gpg not found — cannot verify the feed signature. Install gnupg2." >&2
    return 1
  fi
  if ! verify_manifest "$sums" "$sig"; then
    echo "!! the feed's SHA256SUMS is NOT signed by packages@unom.io (AF245C506F4E4763)." >&2
    echo "!! Someone has tampered with the feed, or the signing key was rotated and this script is" >&2
    echo "!! older than the rotation. Do not install; re-download punktfunk-sysext.sh and retry." >&2
    return 1
  fi
  # Signed — by us, at some point, for something. The two facts a signature cannot carry on its own
  # are stamped inside the document (see the Trust note at the top) and checked here.
  want="$(feed_name)"
  got="$(sed -n 's/^# FEED //p' "$sums" | head -n1)"
  serial="$(sed -n 's/^# SERIAL //p' "$sums" | head -n1)"
  case "$serial" in ''|*[!0-9]*) serial="" ;; esac
  if [ -z "$got" ] || [ -z "$serial" ]; then
    echo "!! the feed $feed is signed but UNBOUND: its manifest carries no '# FEED'/'# SERIAL'" >&2
    echo "!! header, so the signature says nothing about which feed or which publish it covers." >&2
    echo "!! (a feed published before binding existed; it is bound on the next publish, or now with" >&2
    echo "!!  TOKEN=… bash packaging/bazzite/publish-sysext-feed.sh --seal $want)" >&2
    return 1
  fi
  if [ "$got" != "$want" ]; then
    echo "!! this manifest was signed for the feed '$got', but it is being served as '$want' —" >&2
    echo "!! another channel's (or another OS release's) feed is being replayed here. Refusing." >&2
    return 1
  fi
  floor="$(serial_floor)"
  if [ "$serial" -lt "$floor" ]; then
    echo "!! manifest serial $serial is older than the last accepted $floor — refusing rollback." >&2
    echo "!! An old but validly-signed manifest is being replayed at $feed." >&2
    return 1
  fi
  if [ "$serial" -gt "$floor" ]; then raise_serial_floor "$serial"; fi
  return 0
}

# latest -> "VERSION FILENAME SHA256" for the newest image in the VERIFIED manifest (version sort).
#   Call fetch_manifest first — reading $PF_TMP/SHA256SUMS directly is what keeps the signature
#   check off the subshell path, where an `exit` would have vanished into a command substitution.
latest() {
  awk '$2 ~ /^punktfunk-.*-x86-64\.raw$/ { v=$2; sub(/^punktfunk-/,"",v); sub(/-x86-64\.raw$/,"",v); print v, $2, $1 }' \
    "$PF_TMP/SHA256SUMS" | sort -V | tail -n1
}

installed_version() {
  if [ -f "$MARKER" ]; then
    sed -n 's/^SYSEXT_VERSION_ID=//p' "$MARKER"
  elif [ -f "$SIDECAR" ]; then
    cat "$SIDECAR"
  fi
}
merged() { [ -f "$MARKER" ]; }

# True when host.env turns Attach mode on, by the host's own grammar: PUNKTFUNK_GAMESCOPE_ATTACH set
# to anything but 0/false/off/no, or PUNKTFUNK_GAMESCOPE_NODE set at all. Last line wins.
host_env_pins_attach() {
  local attach node
  attach="$(sed -n 's/^[[:space:]]*PUNKTFUNK_GAMESCOPE_ATTACH=[[:space:]]*//p' "$1" | tail -n1 \
            | tr -d "\"'" | tr '[:upper:]' '[:lower:]')"
  node="$(sed -n 's/^[[:space:]]*PUNKTFUNK_GAMESCOPE_NODE=[[:space:]]*//p' "$1" | tail -n1 | tr -d "\"'")"
  case "$attach" in ''|0|false|off|no) [ -n "$node" ] ;; *) return 0 ;; esac
}

post_merge() {
  if ! merged; then
    echo "!! image installed but NOT merged — 'systemd-sysext status' / 'journalctl -u systemd-sysext'" >&2
    echo "!! (an OS release the image doesn't match? 'punktfunk-sysext update' fetches the right one)" >&2
    return 1
  fi
  # The host state every package format shares: groups, udev, sysctl, the seat directories, the
  # ds_inhibit SELinux module, firewall notes. An image older than that helper carries the same
  # steps in its own copy of this script.
  if [ -x /usr/libexec/punktfunk/post-install ]; then
    /usr/libexec/punktfunk/post-install
  else
    /usr/bin/punktfunk-sysext reapply || :
  fi
  if [ -f /usr/lib/sysctl.d/99-punktfunk-client-net.conf ]; then
    sysctl -q -p /usr/lib/sysctl.d/99-punktfunk-client-net.conf 2>/dev/null || :
  fi
  # polkitd keeps the rules it read at boot and misses a merge swapping /usr under it, so a rule
  # an update adds (the door switch's) refuses until reboot. Restarting it rereads them.
  systemctl try-restart polkit.service 2>/dev/null || :
  # vhci-hcd: the usbip transport that makes the virtual Steam Deck pad a real USB device Steam
  # Input adopts. Without it the pad falls back to plain UHID hid-steam, which Steam Input won't
  # promote (Interface: -1) — so on a host in Game Mode the controller never appears and you can't
  # navigate. Two things must be true at boot: the module loaded, and the vhci `attach`/`detach`
  # sysfs files opened to the `input` group (the host runs unprivileged and can't modprobe/chown).
  #
  # A sysext CANNOT rely on its own /usr/lib/modules-load.d + /usr/lib/udev files for this: the
  # image merges (systemd-sysext.service) AFTER systemd-modules-load and early udev have already
  # run, so at a plain reboot vhci-hcd is never loaded and its rule never applied. Mirror BOTH into
  # real /etc (read at the normal early-boot time, and shadowing the /usr copies by filename) so the
  # module loads early and udev's coldplug trigger grants the group access. Refreshed every merge so
  # a rule/module change in a new image propagates (neither is user-editable config). Then load +
  # (re)apply now, no reboot, for this session.
  install -Dm0644 /usr/lib/modules-load.d/punktfunk.conf /etc/modules-load.d/punktfunk.conf 2>/dev/null || :
  install -Dm0644 /usr/lib/udev/rules.d/60-punktfunk.rules /etc/udev/rules.d/60-punktfunk.rules 2>/dev/null || :
  udevadm control --reload 2>/dev/null || :
  # Creating the group is necessary but NOT sufficient, and the difference is invisible until a
  # stream fails: `pf-dm-helper` gates on MEMBERSHIP, so a host whose user never joined gets
  # "stopping the display manager needs privilege" on every managed takeover — sddm's autologin
  # Relogin loop then churns logind sessions for the whole stream. Joining stays opt-in (writing
  # vhci `attach` materialises an arbitrary emulated USB device), so say so instead of doing it.
  local _pf_user="${SUDO_USER:-}"
  if [ -n "$_pf_user" ] && ! id -nG "$_pf_user" 2>/dev/null | tr ' ' '\n' | grep -qx punktfunk; then
    echo "!! $_pf_user is not in the 'punktfunk' group — the managed gamescope takeover cannot stop"
    echo "!! the display manager, and the virtual Steam Deck pad cannot attach. To opt in:"
    echo "!!     sudo usermod -aG punktfunk $_pf_user"
  fi
  # A host.env that turns Attach mode on serves every client a mirror of this box's screen at its
  # own resolution. The file outlives the template it was copied from, so check it on every merge.
  # The console updater has no SUDO_USER; getent then exits 2, which set -e must not see.
  local _pf_env
  _pf_env="$(getent passwd "${_pf_user:-}" 2>/dev/null | cut -d: -f6 || :)/.config/punktfunk/host.env"
  if [ -n "$_pf_user" ] && [ -f "$_pf_env" ] && host_env_pins_attach "$_pf_env"; then
    echo "!! $_pf_env turns Attach mode on: every client gets a mirror of this box's screen at its"
    echo "!! own resolution instead of a display of its own. Delete the PUNKTFUNK_GAMESCOPE_ATTACH"
    echo "!! (or _NODE) line, then: systemctl --user restart punktfunk-host"
  fi
  modprobe vhci-hcd 2>/dev/null || :
  # Re-fire the vhci rule against the (possibly already-present) controller so attach/detach pick up
  # the input-group ownership even when the module's original add event predated the reloaded rule.
  udevadm trigger --subsystem-match=platform --sysname-match='vhci_hcd.*' 2>/dev/null || :
  # The /etc payload a sysext can't carry. The gamescope-session drop-in is %config(noreplace):
  # only seed it, never clobber a local edit. Older images also copied a global tray autostart
  # entry; the host now writes a per-user one.
  if [ -f "$ETC_SRC/gamescope-session-plus/sessions.d/steam" ] \
     && [ ! -e /etc/gamescope-session-plus/sessions.d/steam ]; then
    install -Dm0644 "$ETC_SRC/gamescope-session-plus/sessions.d/steam" \
      /etc/gamescope-session-plus/sessions.d/steam
  fi
  rm -f /etc/xdg/autostart/io.unom.Punktfunk.Tray.desktop
}

# do_install VERSION FILENAME SHA256 | do_install --from-file X.raw
do_install() {
  need_root
  mkdir -p "$EXT_DIR"
  local tmp="$EXT_DIR/.punktfunk.raw.new" ver
  if [ "$1" = --from-file ]; then
    ver="(local: $(basename "$2"))"
    cp -f "$2" "$tmp"
  else
    ver="$1"
    echo "downloading punktfunk $ver ($(channel), fedora $(os_version_id))…"
    curl -fL --progress-bar -o "$tmp" "$(feed_url)/$2"
    echo "$3  $tmp" | sha256sum -c --quiet
  fi
  mv -f "$tmp" "$IMG"          # marker inside is extension-release.punktfunk — name must match
  echo "$ver" > "$SIDECAR"
  systemctl enable --now systemd-sysext.service >/dev/null 2>&1 || :
  systemd-sysext refresh
  post_merge
  echo "punktfunk $ver merged into /usr."
}

layering_hint() {
  if command -v rpm-ostree >/dev/null 2>&1 \
     && rpm-ostree status 2>/dev/null | grep -q 'LayeredPackages:.*punktfunk'; then
    cat >&2 <<'EOF'
!! punktfunk is ALSO layered via rpm-ostree. The sysext now shadows it, but remove the
!! layer so it stops slowing/blocking OS updates (the reason this sysext exists):
!!     sudo rpm-ostree uninstall punktfunk punktfunk-web && systemctl reboot
EOF
  fi
}

cmd_install() {
  need_root
  local from_file=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --channel)   printf 'CHANNEL=%s\n' "${2:?}" > "$CONF"; shift 2 ;;
      --from-file) from_file="${2:?}"; shift 2 ;;
      *) usage 1 ;;
    esac
  done
  if [ -n "$from_file" ]; then
    do_install --from-file "$from_file"
  else
    fetch_manifest || exit 1
    local l; l="$(latest)"
    [ -n "$l" ] || { echo "no image in the feed $(feed_url)" >&2; exit 1; }
    # shellcheck disable=SC2086
    do_install $l
  fi
  layering_hint
  cat <<'EOF'

First-run (once):
  ujust add-user-to-input-group add    # virtual gamepads; then log out + back in
  systemctl --user daemon-reload && systemctl --user enable --now punktfunk-host punktfunk-web
Settings: the console, Host -> Settings. A line in ~/.config/punktfunk/host.env locks that
          setting there until you delete it (annotated template: /usr/share/punktfunk/host.env.bazzite).
Updates:  sudo punktfunk-sysext update
EOF
}

# A console left running after the merge serves the old build's asset names, which /usr no longer has.
# SUDO_USER is the person who ran this. The in-console updater runs it from a root unit without one
# and restarts all three itself once the helper returns. try-restart leaves an opted-out runner off.
restart_user_units() {
  if [ -n "${SUDO_USER:-}" ] \
     && systemctl --user -M "$SUDO_USER@" try-restart punktfunk-web.service punktfunk-host.service; then
    systemctl --user -M "$SUDO_USER@" try-restart punktfunk-scripting.service 2>/dev/null || true
    echo "restarted punktfunk-web, punktfunk-host and the plugin runner for $SUDO_USER."
  else
    echo "restart the console, host and plugin runner to pick up the new build:"
    echo "    systemctl --user restart punktfunk-web punktfunk-host"
    echo "    systemctl --user try-restart punktfunk-scripting"
  fi
}

cmd_update() {
  need_root
  if [ "${1:-}" = --from-file ]; then do_install --from-file "${2:?}"; restart_user_units; return; fi
  local cur l ver
  cur="$(installed_version)"
  fetch_manifest || exit 1
  l="$(latest)"
  [ -n "$l" ] || { echo "no image in the feed $(feed_url)" >&2; exit 1; }
  ver="${l%% *}"
  if [ "$ver" = "$cur" ] && merged; then
    # NOT "nothing to do": re-run post_merge. Every step in it is idempotent, and skipping it here
    # is how host state silently rots one release behind the image.
    #
    # The trap, field-proven on a Bazzite host that took 0.25.0 -> 0.26.0 (2026-08-09): an upgrade
    # is driven by the script from the OLD image — this file is replaced by the very
    # `systemd-sysext refresh` that runs mid-upgrade — so a post_merge step ADDED in the new
    # release is executed by nobody. The old script doesn't have it, and the new script never gets
    # a turn, because from then on `update` matches this branch and returns. The step is then
    # permanently unreachable on exactly the installs that need it.
    #
    # That cost the `punktfunk` group (added to post_merge in 0.26.0): it was never created, so
    # `pf-dm-helper` refused every caller — it gates on membership — and every managed gamescope
    # takeover fell back to "stopping the display manager needs privilege", leaving sddm's autologin
    # Relogin loop churning for the whole stream.
    echo "already on $cur (channel $(channel)) — re-applying host state."
    post_merge
    return
  fi
  # Even a correctly bound, in-date manifest can offer only OLDER images (a mistaken republish, an
  # over-eager prune). `latest` reports the newest the feed HAS, not the newest that ever shipped,
  # so without this the box walks backwards onto a superseded — possibly known-vulnerable —
  # release, silently. Rolling back stays possible; it just has to be asked for.
  case "$cur" in
    [0-9]*)
      if [ "$ver" != "$cur" ] \
         && [ "$(printf '%s\n%s\n' "$ver" "$cur" | sort -V | tail -n1)" = "$cur" ] \
         && [ "${PUNKTFUNK_SYSEXT_ALLOW_DOWNGRADE:-0}" != 1 ]; then
        echo "!! the feed's newest image ($ver) is OLDER than the installed $cur — refusing to" >&2
        echo "!! downgrade. For a deliberate rollback:" >&2
        echo "!!     sudo PUNKTFUNK_SYSEXT_ALLOW_DOWNGRADE=1 punktfunk-sysext update" >&2
        exit 1
      fi ;;
  esac
  echo "updating: ${cur:-<none>} -> $ver"
  # shellcheck disable=SC2086
  do_install $l
  restart_user_units
}

cmd_status() {
  echo "channel:    $(channel)"
  echo "feed:       $(feed_url)"
  echo "image:      $([ -f "$IMG" ] && du -h "$IMG" | cut -f1 || echo '(not installed)')"
  echo "merged:     $(merged && echo yes || echo no)"
  echo "installed:  $(installed_version || true)"
  # Say WHY the feed is unreadable rather than printing a blank: unreachable and
  # "signature does not verify" want very different reactions from whoever ran this.
  if fetch_manifest 2>"$PF_TMP/status.err"; then
    echo "latest:     $(latest | cut -d' ' -f1)"
  else
    echo "latest:     (unavailable)"
  fi
  # Unconditionally — fetch_manifest also warns on SUCCESS (ALLOW_UNSIGNED), and a status command
  # that hides "this feed is not being verified" is worse than one that prints nothing at all.
  [ -s "$PF_TMP/status.err" ] && sed 's/^/            /' "$PF_TMP/status.err" >&2
  return 0
}

cmd_remove() {
  need_root
  # /etc cleanup needs the /usr payload for the unmodified-compare — do it BEFORE unmerging.
  if merged; then
    if cmp -s "$ETC_SRC/gamescope-session-plus/sessions.d/steam" \
              /etc/gamescope-session-plus/sessions.d/steam 2>/dev/null; then
      rm -f /etc/gamescope-session-plus/sessions.d/steam
    fi
  fi
  rm -f /etc/xdg/autostart/io.unom.Punktfunk.Tray.desktop
  # $FLOOR_FILE deliberately survives: it is anti-rollback state, not installation state, and a
  # remove/re-install cycle is the obvious way to hand a box a replayed manifest it already refused.
  rm -f "$IMG" "$SIDECAR" "$CONF"
  systemd-sysext refresh 2>/dev/null || :
  echo "punktfunk sysext removed (user config in ~/.config/punktfunk is untouched)."
}

case "${1:-}" in
  install) shift; cmd_install "$@" ;;
  update)  shift; cmd_update "$@" ;;
  reapply) shift; need_root; post_merge ;;
  status)  shift; cmd_status ;;
  remove)  shift; cmd_remove ;;
  *) usage ;;
esac
