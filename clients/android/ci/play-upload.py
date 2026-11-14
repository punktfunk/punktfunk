#!/usr/bin/env python3
"""Upload a signed AAB to Google Play via the Publishing API — a direct replacement for
r0adkll/upload-google-play, which swallows real API errors into "Unknown error occurred."

Why hand-rolled: stdlib + `openssl` only (no pip on the runner), and it prints Google's actual
error at the stage it fails instead of a catch-all. Reuses the SERVICE_ACCOUNT_JSON secret and
tolerates it being raw JSON *or* base64-encoded JSON.

Usage (upload a new build):
  SERVICE_ACCOUNT_JSON='<raw-or-base64 SA key>' \
    python3 play-upload.py --package io.unom.punktfunk \
      --aab path/to/app-release.aab --track beta --also-track alpha \
      --status completed [--no-commit] [--skip-if-in-review]

Usage (promote a build that is already on Play, no rebuild):
  python3 play-upload.py --package io.unom.punktfunk \
      --promote 10816 --promote-from alpha --track production

Promotion assigns an EXISTING versionCode to another track, so what ships to production is the
byte-identical artifact the testers ran — rebuilding would burn a fresh versionCode and ship
something nobody has tested. --promote-from additionally asserts the code really is on that track
(catches a typo'd versionCode before it reaches production) and empties it in the SAME edit, so
the move is atomic: testers are never left pinned to a code that production also serves.

--no-commit: do insert -> upload/assign -> track-update -> validate, then delete the edit
(publishes nothing). Use it to dry-run the credentials/AAB/notes without touching the live track.

--skip-if-in-review: while an earlier submission is in Play review, delete the edit and exit 0.
Without it the commit cancels that review and starts a new one.
"""
import argparse, base64, json, os, subprocess, sys, tempfile, time
import urllib.request, urllib.parse, urllib.error

API = "https://androidpublisher.googleapis.com/androidpublisher/v3/applications"
UPLOAD = "https://androidpublisher.googleapis.com/upload/androidpublisher/v3/applications"


class ApiError(Exception):
    def __init__(self, code, method, url, body):
        super().__init__(f"HTTP {code} from {method} {url}\n{body}")
        self.code, self.body = code, body


def _b64url(raw: bytes) -> str:
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


def call(method, url, token=None, data=None, content_type=None, want_json=True):
    headers = {}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    if content_type:
        headers["Content-Type"] = content_type
    # Transient-fault retries: googleapis.com occasionally drops the TLS session ("EOF
    # occurred in violation of protocol" — failed two release uploads on 2026-07-02) or
    # answers 5xx. Retry those with backoff; 4xx raises immediately (a real API error).
    # The edits API is transactional until commit, so re-sending any of these is safe.
    last = None
    for attempt in range(4):
        if attempt:
            delay = 3**attempt
            print(f"transient Play API failure ({last}); retry {attempt}/3 in {delay}s")
            time.sleep(delay)
        req = urllib.request.Request(url, data=data, method=method, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=300) as r:
                body = r.read()
            return json.loads(body) if (want_json and body) else body
        except urllib.error.HTTPError as e:
            if e.code >= 500:
                last = f"HTTP {e.code}"
                continue
            raise ApiError(e.code, method, url, e.read().decode("utf-8", "replace"))
        except urllib.error.URLError as e:
            last = str(getattr(e, "reason", e))
            continue
    sys.exit(f"ERROR: {method} {url} still failing after retries: {last}")


def load_sa():
    raw = os.environ.get("SERVICE_ACCOUNT_JSON", "")
    if not raw.strip():
        sys.exit("ERROR: SERVICE_ACCOUNT_JSON env is empty")
    try:                                        # raw JSON (what r0adkll expects)
        return json.loads(raw)
    except json.JSONDecodeError:
        try:                                    # or base64-encoded JSON (common mistake)
            sa = json.loads(base64.b64decode(raw))
            print("note: SERVICE_ACCOUNT_JSON was base64-encoded; decoded it.")
            return sa
        except Exception:
            sys.exit("ERROR: SERVICE_ACCOUNT_JSON is neither valid JSON nor base64-encoded JSON")


def access_token(sa) -> str:
    now = int(time.time())
    header = _b64url(json.dumps({"alg": "RS256", "typ": "JWT"}).encode())
    claims = _b64url(json.dumps({
        "iss": sa["client_email"],
        "scope": "https://www.googleapis.com/auth/androidpublisher",
        "aud": sa["token_uri"], "iat": now, "exp": now + 3600,
    }).encode())
    signing_input = f"{header}.{claims}".encode()
    with tempfile.NamedTemporaryFile("w", suffix=".pem", delete=False) as f:
        f.write(sa["private_key"])
        keyfile = f.name
    try:
        sig = subprocess.run(["openssl", "dgst", "-sha256", "-sign", keyfile],
                             input=signing_input, capture_output=True, check=True).stdout
    finally:
        os.unlink(keyfile)
    jwt = f"{header}.{claims}.{_b64url(sig)}"
    tok = call("POST", sa["token_uri"],
               data=urllib.parse.urlencode({
                   "grant_type": "urn:ietf:params:oauth:grant-type:jwt-bearer",
                   "assertion": jwt}).encode(),
               content_type="application/x-www-form-urlencoded")
    return tok["access_token"]


# Play's "What's new" is capped at 500 characters per language. The cap lives in the Console
# (the REST reference does not state it) and the API rejects longer text at commit — i.e. AFTER
# the AAB has uploaded — so check it up front and print the actual count.
NOTES_MAX = 500


def load_release_notes(path, language):
    with open(path, encoding="utf-8") as f:
        text = f.read().strip()
    if not text:
        sys.exit(f"ERROR: release-notes file is empty: {path}")
    if len(text) > NOTES_MAX:
        sys.exit(f"ERROR: release notes are {len(text)} chars, Play allows {NOTES_MAX}: {path}")
    print(f"release notes: {len(text)}/{NOTES_MAX} chars ({language})")
    return [{"language": language, "text": text}]


def put_track(app, edit, tok, track, version_codes, status, user_fraction=None, notes=None):
    """PUT one track. An empty version_codes list clears the track (what promotion does to the
    track it promoted OUT of)."""
    release = {"status": status, "versionCodes": [str(v) for v in version_codes]}
    if user_fraction is not None:
        release["userFraction"] = user_fraction
    if notes:
        release["releaseNotes"] = notes
    # Clearing a track means "no active releases", not "an empty release".
    body = {"track": track, "releases": [release] if version_codes else []}
    call("PUT", f"{app}/edits/{edit}/tracks/{track}", token=tok,
         data=json.dumps(body).encode(), content_type="application/json")


def assert_on_track(app, edit, tok, track, vc):
    """Fail before anything is written if --promote names a versionCode that is not actually on
    the track we claim to be promoting out of."""
    got = call("GET", f"{app}/edits/{edit}/tracks/{track}", token=tok)
    live = [c for r in got.get("releases", []) for c in r.get("versionCodes", [])]
    if str(vc) not in live:
        sys.exit(f"ERROR: versionCode {vc} is not on track '{track}' (it has: {live or 'nothing'})")
    print(f"verified versionCode={vc} is live on '{track}'")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--package", required=True)
    ap.add_argument("--aab", help="upload this bundle (mutually exclusive with --promote)")
    ap.add_argument("--promote", type=int, metavar="VERSIONCODE",
                    help="assign an already-uploaded versionCode instead of uploading")
    ap.add_argument("--promote-from", metavar="TRACK",
                    help="with --promote: assert the code is on TRACK, then clear TRACK")
    ap.add_argument("--track", default="internal")
    ap.add_argument("--also-track", action="append", default=[], metavar="TRACK",
                    help="assign the same versionCode to this track too, in the same edit "
                         "(repeatable). Canary uses it to feed open + closed testing at once.")
    ap.add_argument("--status", default="completed")
    ap.add_argument("--user-fraction", type=float,
                    help="staged rollout fraction, 0<f<1; required by --status inProgress")
    ap.add_argument("--release-notes-file", help="Play 'What's new' text (<=500 chars)")
    ap.add_argument("--release-notes-language", default="en-US")
    ap.add_argument("--no-commit", action="store_true")
    ap.add_argument("--skip-if-in-review", action="store_true",
                    help="drop this edit instead of restarting a review already in progress")
    a = ap.parse_args()

    if bool(a.aab) == bool(a.promote):
        sys.exit("ERROR: pass exactly one of --aab (upload) or --promote (assign an existing code)")
    if a.promote_from and not a.promote:
        sys.exit("ERROR: --promote-from only applies to --promote")
    # inProgress without a fraction is an API error; halted/completed with one is also rejected.
    if a.status == "inProgress" and a.user_fraction is None:
        sys.exit("ERROR: --status inProgress requires --user-fraction")
    if a.user_fraction is not None and not (0 < a.user_fraction < 1):
        sys.exit(f"ERROR: --user-fraction must be strictly between 0 and 1 (got {a.user_fraction})")
    if a.aab and not os.path.isfile(a.aab):
        sys.exit(f"ERROR: AAB not found: {a.aab}")
    for t in a.also_track:
        # `--also-track <promote-from>` would assign and clear the same track in one edit;
        # whichever PUT lands second silently wins. Refuse the ambiguity instead.
        if t in (a.track, a.promote_from):
            sys.exit(f"ERROR: --also-track {t} duplicates --track/--promote-from")

    notes = load_release_notes(a.release_notes_file, a.release_notes_language) \
        if a.release_notes_file else None

    sa = load_sa()
    tok = access_token(sa)
    print(f"authenticated as {sa['client_email']} (project {sa.get('project_id')})")
    app = f"{API}/{a.package}"

    try:
        edit = call("POST", f"{app}/edits", token=tok)["id"]
        if a.promote:
            vc = a.promote
            if a.promote_from:
                assert_on_track(app, edit, tok, a.promote_from, vc)
        else:
            with open(a.aab, "rb") as f:
                blob = f.read()
            print(f"uploading {a.aab} ({len(blob)} bytes) ...")
            vc = call("POST", f"{UPLOAD}/{a.package}/edits/{edit}/bundles?uploadType=media",
                      token=tok, data=blob, content_type="application/octet-stream")["versionCode"]
            print(f"uploaded versionCode={vc}")

        put_track(app, edit, tok, a.track, [vc], a.status, a.user_fraction, notes)
        print(f"assigned versionCode={vc} -> track={a.track} status={a.status}"
              + (f" userFraction={a.user_fraction}" if a.user_fraction is not None else ""))
        # Same edit, so one commit (and one Play review) covers every track the code lands on —
        # the tracks can never disagree about which canary is current.
        for t in a.also_track:
            put_track(app, edit, tok, t, [vc], a.status, a.user_fraction, notes)
            print(f"assigned versionCode={vc} -> track={t} status={a.status}")
        # Same edit as the assignment above, so the code is never active on both tracks at once.
        if a.promote_from:
            put_track(app, edit, tok, a.promote_from, [], a.status)
            print(f"cleared track '{a.promote_from}'")

        if a.no_commit:
            call("POST", f"{app}/edits/{edit}:validate", token=tok)
            print("validated (dry-run) OK — deleting edit, nothing published")
            call("DELETE", f"{app}/edits/{edit}", token=tok, want_json=False)
            return

        # Play's default commit cancels the submission in review and restarts it with this one.
        commit = f"{app}/edits/{edit}:commit?changesInReviewBehavior=" + (
            "ERROR_IF_IN_REVIEW" if a.skip_if_in_review else "CANCEL_IN_REVIEW_AND_SUBMIT")
        try:
            try:
                call("POST", commit, token=tok)
            except ApiError as e:
                if "changesNotSentForReview" not in e.body:
                    raise
                print("commit needs changesNotSentForReview=true — retrying")
                call("POST", f"{commit}&changesNotSentForReview=true", token=tok)
        except ApiError as e:
            if not (a.skip_if_in_review and "CHANGES_ALREADY_IN_REVIEW" in e.body):
                raise
            call("DELETE", f"{app}/edits/{edit}", token=tok, want_json=False)
            print(f"SKIPPED: an earlier submission is still in Play review; versionCode={vc} not committed")
            return
        print(f"COMMITTED: versionCode={vc} live on track '{a.track}' ({a.status})")
    except ApiError as e:
        print(f"\nPLAY API ERROR:\n{e}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
