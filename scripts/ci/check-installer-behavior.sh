#!/bin/sh
# Behavioural gates for the guided installer, driven against the BINARY.
#
# scripts/install.sh is a stub that downloads punktfunk-setup (design/installer-v2.md D1), so the
# behaviour lives in the binary. These gates are not in check-docs-drift.sh because that gate
# runs in a bun container with no cargo, and this needs a built installer.
#
# Two test seams: PUNKTFUNK_INSTALL_OS_RELEASE fakes the distro, PUNKTFUNK_INSTALL_ETC fakes the
# repo config.
#
# Exit: 0 all cases held · 1 a case drifted · 2 the installer could not be built.
set -u
cd "$(dirname "$0")/../.." || exit 2

# CI passes a built binary; a developer running this by hand gets a debug build.
PF=${PUNKTFUNK_SETUP_BIN:-}
if [ -z "$PF" ]; then
    cargo build -q -p punktfunk-setup || exit 2
    PF=target/debug/punktfunk-setup
fi
[ -x "$PF" ] || { echo "::error::$PF is not executable"; exit 2; }

fail=0
osr=$(mktemp -d)
sw=$(mktemp -d)
trap 'rm -rf "$osr" "$sw"' EXIT

# ------------------------------------------------------------------ the detection matrix
# Faked os-release files through the real installer, nothing executed: each family must be
# detected and print its own package-manager line, both for the install and for --uninstall;
# the unsupported ones must stop with their pointer. A fix to the installer adds its case here.
installer_case() {   # name os-release-body expected-substring [extra args...]
    name=$1; printf '%b' "$2" > "$osr/$name"; want=$3; shift 3
    out=$(PUNKTFUNK_INSTALL_OS_RELEASE="$osr/$name" "$PF" --dry-run --yes --no-start "$@" 2>&1)
    case "$out" in *"$want"*) ;; *)
        echo "::error::punktfunk-setup --dry-run $* on a fake $name os-release did not print '$want':"
        printf '%s\n' "$out" | sed 's/^/    /'
        fail=1 ;;
    esac
}
installer_case debian   'ID=debian\nVERSION_ID=13\n'                    'sudo apt install -y punktfunk-host punktfunk-web punktfunk-scripting'
installer_case ubuntu   'ID=ubuntu\nID_LIKE=debian\nVERSION_ID=26.04\n'  'sudo apt install -y punktfunk-host punktfunk-web punktfunk-scripting'
installer_case mint22   'ID=linuxmint\nID_LIKE="ubuntu debian"\nVERSION_ID=22.1\n' "can't host"
installer_case fedora   'ID=fedora\nVERSION_ID=44\n'                    'sudo dnf install -y punktfunk punktfunk-web punktfunk-scripting'
installer_case fedora43 'ID=fedora\nVERSION_ID=43\n'                    '/rpm/bazzite'
installer_case arch     'ID=arch\n'                                    'sudo pacman -Syu --noconfirm punktfunk-host punktfunk-web punktfunk-scripting'
installer_case cachyos  'ID=cachyos\nID_LIKE="arch"\n'                  'sudo pacman -Syu --noconfirm punktfunk-host punktfunk-web punktfunk-scripting'
# Omarchy: arch family, but its libalpm guard kills any -S+-u transaction, so the install must
# split into -Sy then -S (and --yes still has to reach that -S), and the run must hand off to
# `punktfunk-omarchy setup` rather than do a second, weaker version of the same wiring.
installer_case omarchy  'ID=omarchy\nID_LIKE=arch\nVERSION_ID=4.0.1\n'    'sudo pacman -S --noconfirm punktfunk-host punktfunk-web punktfunk-scripting'
installer_case omarchy2 'ID=omarchy\nID_LIKE=arch\nVERSION_ID=4.0.1\n'    'punktfunk-omarchy setup'
installer_case bazzite  'ID=bazzite\nID_LIKE="fedora"\nVERSION_ID=43\n' 'punktfunk-sysext.sh install'
installer_case nixos    'ID=nixos\n'                                   'docs/nixos'
# SteamOS says ID_LIKE=arch, so it has to beat the arch test or a Deck gets pacman commands its
# read-only /usr cannot run. The install is the on-device build, and a re-run must survive the
# ~/punktfunk that the first one cloned.
installer_case steamos  'ID=steamos\nID_LIKE=arch\n'                    'bash ~/punktfunk/scripts/steamdeck/install.sh'
installer_case steamos2 'ID=steamos\nID_LIKE=arch\n'                    '[ -d ~/punktfunk/.git ] || git clone'
# A Deck's channel is its checkout's branch: `stable` (moved at each release) unless asked.
installer_case steamos3 'ID=steamos\nID_LIKE=arch\n'                    'git clone --branch stable'
installer_case steamos-canary 'ID=steamos\nID_LIKE=arch\n'              'git clone --branch main' --channel canary
installer_case holoiso  'ID=holoiso\nID_LIKE="steamos arch"\n'          'scripts/steamdeck/install.sh'
installer_case gentoo   'ID=gentoo\n'                                  'build-from-source'
# A distro with no host repo is a dead end for the HOST only: --client takes the flatpak line
# instead of dying (design/installer-v2.md §5).
installer_case gentoo-client 'ID=gentoo\n'  'flatpak install --user' --client
installer_case debian-rm 'ID=debian\nVERSION_ID=13\n'                   'sources.list.d/punktfunk.list' --uninstall
installer_case fedora-rm 'ID=fedora\nVERSION_ID=44\n'                   'yum.repos.d/punktfunk.repo' --uninstall
installer_case arch-rm   'ID=arch\n'                                   '/etc/pacman.conf' --uninstall
installer_case omarchy-rm 'ID=omarchy\nID_LIKE=arch\n'                  'punktfunk-omarchy remove' --uninstall
installer_case bazzite-rm 'ID=bazzite\nID_LIKE="fedora"\nVERSION_ID=43\n' 'punktfunk-sysext remove' --uninstall
# Nothing on SteamOS is a package, so --uninstall must say so instead of claiming a removal.
installer_case steamos-rm 'ID=steamos\nID_LIKE=arch\n'                  'no uninstall script' --uninstall

PUNKTFUNK_SETUP_BIN="$PF" sh scripts/ci/check-install-defaults.sh || fail=1

# ------------------------------------------------------------------- channel switching
# A box that already has all three binaries, on a repo config naming one channel, told --channel
# <the other>: it must rewrite the repo AND re-resolve in a direction the package manager would
# otherwise refuse (canary is always a minor ahead of stable, so canary->stable is a downgrade).
# Two fakes make that reachable under --dry-run: stub binaries on PATH for the "already installed"
# probe, and PUNKTFUNK_INSTALL_ETC pointing at the repo config the box is supposedly on.
mkdir -p "$sw/bin"
for b in punktfunk-host punktfunk-web-server punktfunk-scripting; do
    printf '#!/bin/sh\necho 0.0.0-test\n' > "$sw/bin/$b"; chmod +x "$sw/bin/$b"
done
switch_case() {   # name os-release-body config-path config-body expected-substring [extra args...]
    name=$1; printf '%b' "$2" > "$osr/$name"
    mkdir -p "$sw/$name/$(dirname "$3")"; printf '%b' "$4" > "$sw/$name/$3"; want=$5; shift 5
    out=$(PATH="$sw/bin:$PATH" PUNKTFUNK_INSTALL_OS_RELEASE="$osr/$name" PUNKTFUNK_INSTALL_ETC="$sw/$name" \
          "$PF" --dry-run --yes --no-start "$@" 2>&1)
    case "$out" in *"$want"*) ;; *)
        echo "::error::punktfunk-setup --dry-run $* on an installed $name box did not print '$want':"
        printf '%s\n' "$out" | sed 's/^/    /'
        fail=1 ;;
    esac
}
APT_LIST=etc/apt/sources.list.d/punktfunk.list
DEB13='ID=debian\nVERSION_ID=13\n'
switch_case apt-up   "$DEB13" "$APT_LIST" 'deb [x] https://git.unom.io/api/packages/unom/debian stable main\n' \
    'debian canary main' --channel canary
switch_case apt-down "$DEB13" "$APT_LIST" 'deb [x] https://git.unom.io/api/packages/unom/debian canary main\n' \
    '--allow-downgrades' --channel stable
# A canary box missing one of the three packages, re-run with no --channel at all: the missing
# ones must come from CANARY. Letting the flag's stable default win there rewrites the repo and
# drags the whole box back a channel without ever saying so.
mkdir -p "$sw/partial/bin" "$sw/partial/$(dirname "$APT_LIST")"
printf '#!/bin/sh\necho 0.0.0-test\n' > "$sw/partial/bin/punktfunk-host"
chmod +x "$sw/partial/bin/punktfunk-host"
printf 'deb [x] https://git.unom.io/api/packages/unom/debian canary main\n' > "$sw/partial/$APT_LIST"
printf '%b' "$DEB13" > "$osr/apt-partial"
out=$(PATH="$sw/partial/bin:$PATH" PUNKTFUNK_INSTALL_OS_RELEASE="$osr/apt-partial" \
      PUNKTFUNK_INSTALL_ETC="$sw/partial" "$PF" --dry-run --yes --no-start 2>&1)
case "$out" in *'debian canary main'*) ;; *)
    echo "::error::punktfunk-setup with no --channel, on a canary box missing packages, did not stay on canary:"
    printf '%s\n' "$out" | sed 's/^/    /'
    fail=1 ;;
esac
switch_case dnf-up   'ID=fedora\nVERSION_ID=44\n' etc/yum.repos.d/punktfunk.repo \
    'baseurl=https://git.unom.io/api/packages/unom/rpm/fedora-44\n' 'distro-sync' --channel canary
switch_case pac-down 'ID=arch\n' etc/pacman.conf '[punktfunk-canary]\nServer = x\n' \
    "/^Server = /d' /etc/pacman.conf" --channel stable
switch_case sys-down 'ID=bazzite\nID_LIKE="fedora"\nVERSION_ID=43\n' etc/punktfunk-sysext.conf 'CHANNEL=canary\n' \
    'punktfunk-sysext.sh install --channel stable' --channel stable

exit $fail
