#!/usr/bin/env bash
# Reclaim the disk that Gitea act_runner leaks on this host.
#
# Why this exists: act_runner creates a per-job network and a pair of named volumes, and leaks both
# when a job is killed or the runner restarts. Left alone they exhaust the disk, failing CI with
# "No space left on device" at both the cargo and the Docker/overlayfs layer, and the stale
# networks exhaust Docker's default address pool.
#
# Install it on every runner host, from here.
#
# Install on a runner host (root):
#   install -m755 scripts/ci/docker-reclaim.sh /usr/local/sbin/ci-docker-reclaim.sh
#   install -m644 scripts/ci/docker-reclaim.service /etc/systemd/system/ci-docker-reclaim.service
#   install -m644 scripts/ci/docker-reclaim.timer   /etc/systemd/system/ci-docker-reclaim.timer
#   systemctl daemon-reload && systemctl enable --now ci-docker-reclaim.timer
#
# Deliberately NOT `docker volume prune -a`: that would also delete any intentional named volume
# that merely has no container attached at the moment the timer fires — e.g. the `docker-mirror`
# pull-through registry cache or the runner cache during a restart — silently destroying it. Only
# volumes act_runner named are removed here.
#
# Also deliberately NOT pruning images: on this host the per-SHA CI tags share all their layers with
# `:latest`, so removing them reclaims nothing while forcing re-pulls. `docker system df`'s
# "RECLAIMABLE" column counts shared layers once per image and overstates the win badly.
# (docker-prune.sh owns tag retirement — age-gated and never `image prune -a`, see its header.)
set -uo pipefail

log() { echo "ci-docker-reclaim: $*"; }
DISK=${DISK:-/}  # the filesystem holding Docker's data

before_avail=$(df --output=avail -BM "$DISK" | tail -1 | tr -dc '0-9')

# 1. Leaked per-job volumes — dangling AND named by act_runner. In-use volumes are never listed as
#    dangling, so a running job's volumes cannot be hit.
mapfile -t stale_vols < <(docker volume ls -qf dangling=true 2>/dev/null | grep '^GITEA-ACTIONS-TASK-' || true)
if ((${#stale_vols[@]})); then
    printf '%s\n' "${stale_vols[@]}" | xargs -r docker volume rm >/dev/null 2>&1
    log "removed ${#stale_vols[@]} leaked act_runner volumes"
else
    log "no leaked act_runner volumes"
fi

# 2. Unused networks older than 2h — never touches a live job's network (it is in use), and the age
#    filter keeps a just-created one safe against a race with a starting job.
net_out=$(docker network prune -f --filter until=2h 2>&1 | grep -c '^GITEA-ACTIONS' || true)
log "removed ${net_out:-0} stale job networks"

# 3. Build cache older than 48h. Recent cache is what makes builds fast, so it is kept.
cache_freed=$(docker builder prune -f --filter until=48h 2>&1 | awk '/^Total:/ {print $2}')
log "build cache freed: ${cache_freed:-0B}"

after_avail=$(df --output=avail -BM "$DISK" | tail -1 | tr -dc '0-9')
log "avail ${before_avail}M -> ${after_avail}M (reclaimed $((after_avail - before_avail))M)"
df -h "$DISK" | tail -1 | sed 's/^/ci-docker-reclaim: /'
