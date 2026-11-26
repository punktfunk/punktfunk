#!/usr/bin/env bash
# Detached-GPG-sign the built dist/*.rpm so the Gitea RPM registry can be served with gpgcheck=1.
#
# ACTIVE: RPM_GPG_PRIVATE_KEY is set as an ORG-level secret on `unom` (packages@unom.io,
# AF245C506F4E4763 — the public half is committed at packaging/rpm/RPM-GPG-KEY-punktfunk and served
# from the registry). Every published RPM carries its EdDSA header signature; the repo file in
# README.md tells users gpgcheck=1, which only works because of that.
#
# Without the key this exits 0 and leaves the RPMs unsigned, so a fork or a local build still works
# — but NOT on a release. On refs/tags/v* a missing key is a hard failure: an org secret that got
# rotated, renamed, or not inherited would otherwise publish an unsigned release into a repo whose
# own instructions say gpgcheck=1, and every user's `dnf upgrade` would break on it. Better to fail
# the build than to find out from users. (Same fail-closed rule as the Windows pack scripts.)
#
# Requires a DEDICATED, PASSPHRASE-LESS signing key (the one the runbook generates with
# %no-protection), distinct from the Gitea instance's repo-metadata key — rpm's default signer
# can't supply a passphrase non-interactively here.
#
# Usage (in rpm.yml, after build-rpm.sh): RPM_GPG_PRIVATE_KEY=... bash packaging/rpm/sign-rpms.sh
#
# Takes the RPMs to sign as arguments, defaulting to `dist/*.rpm`. The argument form exists because
# punktfunk-gamescope is built LATER in the job than the host RPMs, behind its own cache, so it
# misses the main signing pass and needs a second one just for itself (rpm.yml). An unsigned
# package in a `gpgcheck=1` repo is in the channel and still uninstallable: `dnf install` fails
# with "The package is not signed".
set -euo pipefail

# Default target, and a real glob rather than a literal when nothing matched.
if [ "$#" -gt 0 ]; then
  RPMS=("$@")
else
  shopt -s nullglob
  RPMS=(dist/*.rpm)
fi
[ "${#RPMS[@]}" -gt 0 ] || { echo "no RPMs to sign" >&2; exit 1; }

if [ -z "${RPM_GPG_PRIVATE_KEY:-}" ]; then
  case "${GITHUB_REF:-}" in
    refs/tags/v*)
      echo "RPM_GPG_PRIVATE_KEY unset on a release build (${GITHUB_REF}) — refusing to publish" >&2
      echo "unsigned RPMs into a gpgcheck=1 repo. Restore the org secret (packaging/rpm/README.md)." >&2
      exit 1 ;;
  esac
  echo "RPM_GPG_PRIVATE_KEY unset — leaving dist/*.rpm UNSIGNED (non-release build)."
  exit 0
fi

command -v rpmsign >/dev/null 2>&1 || dnf -y install rpm-sign >/dev/null

GNUPGHOME="$(mktemp -d)"; export GNUPGHOME; chmod 700 "$GNUPGHOME"
trap 'rm -rf "$GNUPGHOME"' EXIT
# Non-interactive in CI (no TTY): force loopback pinentry via gpg.conf so even rpm's default
# signing macro's gpg call won't try to prompt. The passphrase-less key needs no prompt anyway.
printf 'pinentry-mode loopback\n' > "$GNUPGHOME/gpg.conf"
printf 'allow-loopback-pinentry\n' > "$GNUPGHOME/gpg-agent.conf"

printf '%s' "$RPM_GPG_PRIVATE_KEY" | gpg --batch --import
KEYID="$(gpg --list-secret-keys --with-colons | awk -F: '/^sec:/{print $5; exit}')"
[ -n "$KEYID" ] || { echo "no secret key imported from RPM_GPG_PRIVATE_KEY" >&2; exit 1; }

# Sign with rpm's DEFAULT __gpg_sign_cmd — it expands %{__signature_filename}/%{__plaintext_filename}
# correctly. (A custom __gpg_sign_cmd passed via --define reached gpg with those filename macros
# UNEXPANDED -> "No such file or directory".) Just point rpm at our key; the GNUPGHOME above
# (passphrase-less key + loopback) lets gpg sign headless.
for rpm in "${RPMS[@]}"; do
  rpmsign --define "_gpg_name $KEYID" --addsign "$rpm"
done

# Verify locally so a bad signature fails the build before publishing. The key goes through a
# file: rpm 4.20 (openSUSE) imports nothing from a pipe, and fails with "import read failed(0)".
gpg --export --armor "$KEYID" > "$GNUPGHOME/signing-key.asc"
rpm --import "$GNUPGHOME/signing-key.asc"
rpmkeys --checksig "${RPMS[@]}"
echo "signed + verified ${#RPMS[@]} RPM(s) with key $KEYID"
