#!/bin/sh
# Point this box's apt, dnf or pacman at ci-core's package mirror (unom/infra
# runners/ci-core), so CI fetches each package from the internet once. For CI images and job
# containers only: off the LAN their installs fail. The package managers still check
# signatures, so the plain-HTTP LAN hop is fine.
set -eu
M=http://192.168.1.58:8090

for f in /etc/apt/sources.list /etc/apt/sources.list.d/*.list /etc/apt/sources.list.d/*.sources; do
  if [ -f "$f" ]; then
    sed -i -E "s#https?://(deb\.debian\.org|archive\.ubuntu\.com|security\.ubuntu\.com|ports\.ubuntu\.com)/#$M/\1/#g" "$f"
  fi
done

# Fedora 45 keeps its repos in /usr/share/dnf5/repos.d. The Cisco openh264 repo has no
# baseurl to swap in, so it keeps its metalink.
for f in $(grep -l 'download\.example/pub/fedora' /etc/yum.repos.d/*.repo /usr/share/dnf5/repos.d/*.repo 2>/dev/null); do
  sed -i -E -e 's/^metalink=/#metalink=/' \
    -e "s#^\#?baseurl=https?://download\.example/pub/fedora/linux#baseurl=$M/ftp.fau.de/fedora/linux#" "$f"
done

if [ -f /etc/pacman.d/mirrorlist ]; then
  echo "Server = $M/fastly.mirror.pkgbuild.com/\$repo/os/\$arch" > /etc/pacman.d/mirrorlist
fi
