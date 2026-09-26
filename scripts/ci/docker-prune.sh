#!/usr/bin/env bash
# CI runner disk hygiene — invoked by docker-prune.service (every 2 min). A script, not inline
# ExecStart=, because systemd does its own $-expansion on ExecStart before the shell runs.
#
# Sibling: docker-reclaim.sh (hourly) handles what act_runner leaks. This one retires what CI
# produces and abandons: per-SHA app tags and superseded content-keyed (ck-*) builder images.
#
# Every step that deletes image-store content waits for "no pull in flight": containerd GC drops
# an in-flight pull's leases, and the job then dies at "Set up job" with "No such image".
# Idle base images are never swept wholesale (no `image prune -a`): ci-image-warm keeps them warm
# so a job's pull is a digest-match no-op, and a cold 4–10 GB pull is what that avoids.
set -u
export PATH=/usr/bin:/bin:/usr/local/bin:$PATH

BURST_PCT=${BURST_PCT:-80}      # burst-clear once the disk is this % full...
MIN_FREE_GB=${MIN_FREE_GB:-60}  # ...or this little is left: three concurrent Rust target/
                                # dirs can eat the rest inside one interval.
EMERGENCY_FREE_GB=${EMERGENCY_FREE_GB:-25}  # below this, one killed pull beats every job's ENOSPC
CK_MAX_AGE_H=${CK_MAX_AGE_H:-48}  # a superseded ck-* tag older than this is retired

# containerd (moby namespace) lists an active ingest per pull. No ctr means assume a pull.
pull_in_flight() {
  command -v ctr >/dev/null 2>&1 || return 0
  [ -n "$(ctr -n moby content active 2>/dev/null | tail -n +2)" ]
}

# Per-SHA app tags older than 2 h, and ck-* tags whose image is no longer their repo's :latest
# and is older than CK_MAX_AGE_H. A repo without a local :latest keeps its ck-* tags.
retired_tags() {
  now=$(date +%s)
  docker images --format '{{.Repository}} {{.Tag}} {{.ID}}' 2>/dev/null | while read -r repo tag id; do
    case $tag in
      sha-*) max=7200 ;;
      ck-*)
        latest=$(docker image inspect -f '{{.Id}}' "$repo:latest" 2>/dev/null) || continue
        case $latest in *"$id"*) continue ;; esac
        max=$((CK_MAX_AGE_H * 3600)) ;;
      *) continue ;;
    esac
    created=$(docker image inspect -f '{{.Created}}' "$repo:$tag" 2>/dev/null) || continue
    cts=$(date -d "$created" +%s 2>/dev/null) || continue
    [ $((now - cts)) -ge "$max" ] && echo "$repo:$tag"
  done
}

# 1) Routine. rmi without -f: an image a container still uses stays.
if ! pull_in_flight; then
  retired_tags | xargs -r docker rmi >/dev/null 2>&1 || true
  docker image prune    -f || true
  docker builder prune -af --filter until=2h || true
  docker buildx prune  -af --filter until=2h || true
else
  echo "pull in flight — skipping image/builder GC this tick"
fi
docker container prune -f --filter until=2h || true

# 2) Leaked per-job GITEA-ACTIONS-TASK-* networks exhaust the docker address pool.
#    until=2h protects the networks of live jobs.
docker network prune -f --filter until=2h || true

# 3) Burst guard: free space as a floor as well as a percentage, since the headroom three jobs
#    need does not move when the disk is resized.
PCT=$(df --output=pcent / | tr -dc '0-9')
FREE_GB=$(df --output=avail -BG / | tr -dc '0-9')
BURST=0
[ -n "$PCT" ] && [ "$PCT" -ge "$BURST_PCT" ] && BURST=1
[ -n "$FREE_GB" ] && [ "$FREE_GB" -lt "$MIN_FREE_GB" ] && BURST=1
if [ "$BURST" = 1 ]; then
  echo "disk ${PCT}% used, ${FREE_GB}G free (thresholds ${BURST_PCT}% / ${MIN_FREE_GB}G) — burst clear"
  docker builder prune -af || true
  docker buildx prune -af  || true
  docker volume ls -qf dangling=true 2>/dev/null | grep '^GITEA-ACTIONS-TASK-' | xargs -r docker volume rm >/dev/null 2>&1 || true
  if [ -n "$FREE_GB" ] && [ "$FREE_GB" -lt "$EMERGENCY_FREE_GB" ]; then
    echo "EMERGENCY (<${EMERGENCY_FREE_GB}G free): dangling image sweep despite possible in-flight pull"
    docker image prune -f || true
  fi
fi
