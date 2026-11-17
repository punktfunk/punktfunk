# shellcheck shell=bash
# Shared Gitea Release helpers for the punktfunk CI workflows (Linux + macOS runners).
#
# Source this file, then call ensure_release / upsert_asset. The bare asset POST returns 409
# if an asset with the same name already exists, so re-running a workflow — or reusing the
# rolling `canary` release with stable filenames — would fail. upsert_asset deletes the old
# asset first.
#
# upsert_asset also attaches a `<asset>.sha256` sidecar for every asset, so a download off a
# release page is verifiable (`sha256sum -c punktfunk-1.2.3.dmg.sha256`) — see its comment for
# why sidecars rather than one shared SHA256SUMS.
#
# Callers run under `bash -eo pipefail` or POSIX `sh -e` (see _release_notes_path), so a
# non-zero return from these functions aborts the step (the desired behaviour on a real failure).
#
# Env (Gitea Actions sets the first two automatically in every step):
#   GITHUB_SERVER_URL   e.g. https://git.unom.io
#   GITHUB_REPOSITORY   e.g. unom/punktfunk
#   GITEA_TOKEN         a PAT with repository (release) write scope — set from secrets.REGISTRY_TOKEN
#                       (the same PAT the package uploads use; it must carry `write:repository`,
#                       not only `write:package`, or the release-asset POST 403s)
#
# Requires: curl + python3 (python3 is already a proven dependency on every runner that
# attaches releases today — macOS, the fedora flatpak container, the node:bookworm decky
# image; the .deb runner installs it alongside its other apt deps).

_gitea_api() { printf '%s/api/v1/repos/%s' "${GITHUB_SERVER_URL:?}" "${GITHUB_REPOSITORY:?}"; }

# Tiny JSON / URL helpers. python3 reads the TOP-LEVEL "id" only, so there is no ambiguity
# with the nested author.id / assets[].id fields a string-grep would trip over.
_json_id() { python3 -c 'import json,sys;print(json.load(sys.stdin).get("id",""))' 2>/dev/null; }
_json_asset_id() {
  python3 -c 'import json,sys
want=sys.argv[1]
for a in json.load(sys.stdin):
    if a.get("name")==want:
        print(a.get("id",""));break' "$1" 2>/dev/null
}
_urlencode() { python3 -c 'import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1],safe=""))' "$1"; }
# ⚠ Do NOT add helpers here for a workflow step that runs against a CHECKED-OUT RELEASE TAG
# (arch.yml's release-rebuild dispatch). Callers source this file from the working tree, so such a
# step gets the version of this file that shipped in that tag — never the one you just wrote. That
# logic belongs in the workflow, which is always read from the dispatched ref.

# _release_notes_path TAG
#   Print the path of the in-repo release notes for TAG (docs/releases/<TAG>.md) IFF it exists,
#   else print nothing. This file is the single source of truth for a stable release's body
#   (authored as part of the version bump, before the tag is pushed — see docs/releases/README.md),
#   so the Gitea release is born WITH its notes instead of being PATCHed noteless-then-late.
#   canary/rc tags have no such file, which is intended (they get no curated body).
#
#   Resolved relative to CWD: every caller sources this as `. scripts/ci/gitea-release.sh` from the
#   repo root, so the notes are always docs/releases/<tag>.md from here. Do NOT use ${BASH_SOURCE[0]}
#   — the deb + decky attach steps run under POSIX sh (dash), where an array subscript is a fatal
#   "Bad substitution".
_release_notes_path() {
  local notes="docs/releases/$1.md"
  [ -f "$notes" ] && printf '%s' "$notes"
}

# ensure_release TAG NAME PRERELEASE [TARGET_COMMITISH]
#   Idempotently create (or fetch) the release for TAG; prints its numeric id on stdout.
#   PRERELEASE is "true", "false", or "auto" — auto marks it a prerelease iff TAG carries a
#   `-` pre-release suffix (e.g. v0.2.0-rc1), so an rc never becomes the repo's "Latest" release
#   (Gitea's /releases/latest surfaces the newest non-prerelease). TARGET_COMMITISH (optional)
#   creates the git tag if it does not exist yet — for a `vX.Y.Z` release the tag already exists
#   (it is the trigger), so TARGET is omitted and create-vs-fetch hinges on the release object.
ensure_release() {
  local tag="${1:?tag}" name="${2:?name}" prerelease="${3:?prerelease}" target="${4:-}"
  local api body id
  if [ "$prerelease" = auto ]; then
    case "$tag" in *-*) prerelease=true ;; *) prerelease=false ;; esac
  fi
  api="$(_gitea_api)"
  # Build the create payload with python3 so the (multi-line, quote-bearing) release body from
  # docs/releases/<tag>.md is JSON-escaped correctly. Whichever workflow wins the create race thus
  # sets the body ATOMICALLY at creation; the losers 409 and fall through to the fetch-by-tag path
  # below (which never touches the body). No notes file (canary/rc) -> no body key -> empty body.
  local notes; notes="$(_release_notes_path "$tag")"
  body=$(TAG="$tag" NAME="$name" PRERELEASE="$prerelease" TARGET="$target" NOTES_FILE="$notes" \
    python3 - <<'PY'
import json, os
d = {"tag_name": os.environ["TAG"], "name": os.environ["NAME"],
     "prerelease": os.environ["PRERELEASE"] == "true"}
if os.environ.get("TARGET"):
    d["target_commitish"] = os.environ["TARGET"]
nf = os.environ.get("NOTES_FILE") or ""
if nf:
    with open(nf, encoding="utf-8") as f:
        d["body"] = f.read()
print(json.dumps(d))
PY
)
  # Try to create. On any failure (almost always "release already exists"), fall back to
  # fetching it by tag. Either path MUST yield an id, or we error loudly — so a 401/scope
  # problem can't masquerade as a successful no-op.
  id=$(curl -fsS -X POST "$api/releases" \
         -H "Authorization: token ${GITEA_TOKEN:?}" -H 'Content-Type: application/json' \
         -d "$body" 2>/dev/null | _json_id || true)
  if [ -z "$id" ]; then
    id=$(curl -fsS "$api/releases/tags/$tag" \
           -H "Authorization: token ${GITEA_TOKEN:?}" 2>/dev/null | _json_id || true)
  fi
  if [ -z "$id" ]; then
    echo "gitea-release: could not create or find a release for tag '$tag'" >&2
    return 1
  fi
  printf '%s' "$id"
}

# _put_asset RELEASE_ID FILE NAME
#   The raw attach: delete any existing asset of the same name, then POST FILE under NAME, so
#   re-runs and rolling canary re-uploads are idempotent (a plain POST 409s on a dup name).
_put_asset() {
  local rid="$1" file="$2" name="$3"
  local api existing
  api="$(_gitea_api)"
  existing=$(curl -fsS "$api/releases/$rid/assets" \
               -H "Authorization: token ${GITEA_TOKEN:?}" 2>/dev/null \
             | _json_asset_id "$name" || true)
  if [ -n "$existing" ]; then
    curl -fsS -o /dev/null -X DELETE "$api/releases/$rid/assets/$existing" \
      -H "Authorization: token ${GITEA_TOKEN:?}" || true
  fi
  curl -fsS -o /dev/null -X POST "$api/releases/$rid/assets?name=$(_urlencode "$name")" \
    -H "Authorization: token ${GITEA_TOKEN:?}" \
    -F "attachment=@$file"
  echo "gitea-release: uploaded '$name' -> release $rid"
}

# _sha256 FILE -> lowercase hex digest
#   python3 rather than sha256sum/shasum: python3 is already a hard dependency of this file, so
#   one spelling works on every runner.
_sha256() {
  python3 - "$1" <<'PY'
import hashlib, sys
h = hashlib.sha256()
with open(sys.argv[1], "rb") as f:
    for chunk in iter(lambda: f.read(1 << 20), b""):
        h.update(chunk)
print(h.hexdigest())
PY
}

# upsert_asset RELEASE_ID FILE [NAME]
#   Attach FILE to the release AND a `<NAME>.sha256` checksum sidecar next to it, so every
#   download off a release page can be verified:  sha256sum -c punktfunk-1.2.3.dmg.sha256
#
#   Sidecars, not one shared SHA256SUMS, ON PURPOSE: every platform's workflow (windows-*,
#   android, apple, decky, deb, rpm, arch, flatpak) attaches to the SAME release object
#   concurrently, and a single manifest would be a read-modify-write race that silently loses
#   whichever leg lost. One sidecar per asset has no shared mutable state — each leg only ever
#   writes names it owns.
#
#   Living here rather than in the callers means a new packaging workflow inherits checksums for
#   free; the only way to attach an unchecksummed asset is to bypass this helper entirely.
upsert_asset() {
  local rid="${1:?release id}" file="${2:?file}" name="${3:-}"
  local sums
  [ -n "$name" ] || name="$(basename "$file")"
  [ -f "$file" ] || { echo "gitea-release: asset file not found: $file" >&2; return 1; }
  _put_asset "$rid" "$file" "$name"
  # A sidecar gets no sidecar of its own (and neither does a caller attaching one by hand).
  case "$name" in *.sha256) return 0 ;; esac
  # `sha256sum -c` wants "<digest>  <filename>" with the filename as downloaded — i.e. the ASSET
  # name, which is not necessarily the local basename (callers rename, e.g. Punktfunk-$VERSION.dmg).
  sums="$(mktemp)"
  printf '%s  %s\n' "$(_sha256 "$file")" "$name" > "$sums"
  if _put_asset "$rid" "$sums" "$name.sha256"; then rm -f "$sums"; else rm -f "$sums"; return 1; fi
}


# apply_release_notes RELEASE_ID TAG
#   Force the release body to match docs/releases/<TAG>.md (the source of truth), if that file
#   exists — a no-op otherwise. PATCHes ONLY the body, so name/prerelease/assets are preserved
#   (Gitea has no partial-update footgun here; sending just {"body":...} leaves everything else).
#   ensure_release already seeds the body at creation; this exists so the announce step can
#   re-assert the file over the live release right before publishing — covering the case where the
#   notes file was edited after the release object was first created (e.g. a tag re-point).
apply_release_notes() {
  local rid="${1:?release id}" tag="${2:?tag}" api notes payload
  notes="$(_release_notes_path "$tag")"
  [ -n "$notes" ] || { echo "gitea-release: no docs/releases/$tag.md — leaving body as-is"; return 0; }
  api="$(_gitea_api)"
  payload=$(NOTES_FILE="$notes" python3 -c \
    'import json,os;print(json.dumps({"body":open(os.environ["NOTES_FILE"],encoding="utf-8").read()}))')
  curl -fsS -o /dev/null -X PATCH "$api/releases/$rid" \
    -H "Authorization: token ${GITEA_TOKEN:?}" -H 'Content-Type: application/json' \
    -d "$payload"
  echo "gitea-release: synced release $rid body from $notes"
}
