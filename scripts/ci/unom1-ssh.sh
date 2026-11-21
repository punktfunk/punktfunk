# shellcheck shell=sh
# Sourced by every step that deploys to unom-1: sets $SSH, $SCP and $DEST.
#
# The host key is pinned from DEPLOY_KNOWN_HOSTS (`ssh-keyscan`, packaging/nix/README.md) under
# one HostKeyAlias, so the pin follows the key and not the address. UNOM1_TRUST_NEW=1 is for a
# box deploy-all just provisioned: it accepts that box's new key, as deploy-all itself does.
set -eu
: "${DEPLOY_HOST:?DEPLOY_HOST is not set}"
: "${DEPLOY_USER:?DEPLOY_USER is not set}"
: "${DEPLOY_SSH_KEY:?DEPLOY_SSH_KEY is not set}"
command -v ssh >/dev/null 2>&1 \
    || { apt-get update -qq && apt-get install -y -qq --no-install-recommends openssh-client; }
install -d -m 700 "$HOME/.ssh"
printf '%s\n' "$DEPLOY_SSH_KEY" > "$HOME/.ssh/unom1"
chmod 600 "$HOME/.ssh/unom1"
UNOM1_KNOWN="$HOME/.ssh/unom1_known_hosts"
if [ "${UNOM1_TRUST_NEW:-0}" = 1 ]; then
    : > "$UNOM1_KNOWN"
    UNOM1_CHECK=accept-new
else
    if [ -z "${DEPLOY_KNOWN_HOSTS:-}" ]; then
        echo "::error::DEPLOY_KNOWN_HOSTS is not set. See packaging/nix/README.md." >&2
        exit 1
    fi
    printf '%s\n' "$DEPLOY_KNOWN_HOSTS" \
        | awk '$0 !~ /^[[:space:]]*(#|$)/ { $1 = "unom-1"; print }' > "$UNOM1_KNOWN"
    UNOM1_CHECK=yes
fi
chmod 600 "$UNOM1_KNOWN"
UNOM1_OPTS="-i $HOME/.ssh/unom1 -o IdentitiesOnly=yes -o HostKeyAlias=unom-1 -o UserKnownHostsFile=$UNOM1_KNOWN -o StrictHostKeyChecking=$UNOM1_CHECK -o ConnectTimeout=20"
SSH="ssh $UNOM1_OPTS -p ${DEPLOY_PORT:-22}"
SCP="scp $UNOM1_OPTS -P ${DEPLOY_PORT:-22}"
DEST="$DEPLOY_USER@$DEPLOY_HOST"
