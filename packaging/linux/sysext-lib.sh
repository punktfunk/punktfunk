# shellcheck shell=bash
# Sourced by packaging/{arch,bazzite}/build-sysext.sh: the capability seal, the HDR gamescope
# check and the boot-time sysctl unit, run on the staging tree before mksquashfs. The matrix itself is
# scripts/ci/assert-cap-matrix.sh, the same code CI runs on the finished image.
#
# Why the worker is capped here: a sysext's /usr is a read-only squashfs, no package scriptlet
# ever runs for it, and neither a pacman payload nor `rpm2cpio | cpio` carries a capability. So
# a setcap on the staging tree is the only way the image gets one, and mksquashfs records it.
# The host must never carry one: KWin cannot identify a capability-carrying client, and every
# Desktop-mode session on the merged image dies (packaging/arch/punktfunk-host.install).
. "$(dirname "${BASH_SOURCE[0]}")/../../scripts/ci/assert-cap-matrix.sh"

# Canonical capability string on a staged file, "" for none or no such file.
pf_caps_of() {
  caps_norm "$(getcap "$1" 2>/dev/null | sed 's/^[^ ]* //')"
}

# pf_seal_caps <stage> <why>: refuse a worker capability that arrived from elsewhere (<why> says
# why nothing upstream should grant one), grant the worker cap_sys_nice=ep, then assert the
# matrix. The grant needs CAP_SETFCAP, so an uncapped worker only warns: it still encodes, at
# default GPU priority. An image with neither binary (a client image) passes untouched.
pf_seal_caps() {
  local stage="$1" why="$2" present=0 arrived
  [ -f "$stage/$WORKER_REL" ] && present=1
  [ "$present" = 1 ] || [ -f "$stage/$HOST_REL" ] || return 0
  if [ "$present" = 1 ] && command -v getcap >/dev/null 2>&1; then
    arrived="$(pf_caps_of "$stage/$WORKER_REL")"
    case "$arrived" in
      '' | "$WANT_WORKER_CAPS") ;;
      *)
        err "staged $WORKER_REL ARRIVED carrying '$arrived'. $why"
        err "Find out what granted it: it does the same on the plain package path, unchecked."
        return 1
        ;;
    esac
  fi
  if [ "$present" = 1 ]; then
    if setcap "$WANT_WORKER_CAPS" "$stage/$WORKER_REL" 2>/dev/null; then
      note "granted CAP_SYS_NICE to $WORKER_REL (GPU-priority lever active)"
    else
      echo "WARNING: couldn't setcap CAP_SYS_NICE on $WORKER_REL (needs root or CAP_SETFCAP)" >&2
    fi
  fi
  command -v getcap >/dev/null 2>&1 || return 0
  assert_matrix "staged image" "$(pf_caps_of "$stage/$HOST_REL")" \
    "$(pf_caps_of "$stage/$WORKER_REL")" "$present" uncapped-ok
}

# pf_verify_gamescope <root> <what>: <root> holds usr/bin/punktfunk-gamescope and its WSI layer.
# The binary must print the +pfhdr banner: an unpatched gamescope under this name makes the host
# promise HDR it cannot deliver. Without the layer, the stream is HDR while every game renders SDR.
pf_verify_gamescope() {
  local root="$1" what="$2" f
  [ -x "$root/usr/bin/punktfunk-gamescope" ] || {
    err "$what has no executable usr/bin/punktfunk-gamescope"; return 1; }
  "$root/usr/bin/punktfunk-gamescope" --version 2>&1 | grep -q '+pfhdr' || {
    err "$what: punktfunk-gamescope has no +pfhdr marker — it is not a punktfunk HDR build"
    return 1
  }
  for f in usr/lib/punktfunk/libVkLayer_PUNKTFUNK_gamescope_wsi.so \
           usr/lib/punktfunk/vulkan/implicit_layer.d/punktfunk_gamescope_wsi.json; do
    [ -f "$root/$f" ] || { err "$what has no $f — no game HDR without it"; return 1; }
  done
}

# pf_stage_sysctl_unit <stage>: systemd-sysctl runs before systemd-sysext merges the image, so the
# staged sysctl.d files never apply at boot. This unit applies them after the merge. sysinit.target
# Upholds it because a Wants= that arrives with the post-merge reload never starts anything.
pf_stage_sysctl_unit() {
  local units="$1/usr/lib/systemd/system" files="" f
  for f in "$1"/usr/lib/sysctl.d/99-punktfunk*.conf; do
    [ -f "$f" ] && files="$files ${f##*/}"
  done
  [ -n "$files" ] || return 0
  install -d "$units/sysinit.target.d"
  cat > "$units/punktfunk-sysctl.service" <<UNIT
[Unit]
Description=punktfunk UDP socket buffer limits
DefaultDependencies=no
After=systemd-sysext.service systemd-sysctl.service
Conflicts=shutdown.target
Before=shutdown.target
ConditionPathIsReadWrite=/proc/sys/net/

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/lib/systemd/systemd-sysctl$files
UNIT
  printf '[Unit]\nUpholds=punktfunk-sysctl.service\n' \
    > "$units/sysinit.target.d/50-punktfunk-sysctl.conf"
}
