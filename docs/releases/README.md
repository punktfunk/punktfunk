# Release notes

One file per **stable** release: `docs/releases/vX.Y.Z.md`. Its contents become the Gitea release
body **verbatim** and are the source of the Discord `#releases` announcement.

## Why this exists

Releases used to be created by CI with an **empty body**; the notes were pasted in by hand
afterward. That left a window where the release — and anything announcing it — carried no notes.
Now the notes are authored **before the tag is pushed**, as part of the version bump, so the
release is born complete and the announcement always has something to say.

## The flow

1. **Write the notes.** Run skill `write-release-notes` (rules in `docs/writing.md` §2).
   Add `docs/releases/vX.Y.Z.md` in the same commit (or PR) as the version bump. This file is
   the single source of truth for the body.
   **Docs freshness, while you have the diff in front of you:** every user-facing fact the release
   changes has its docs-site page updated (CONTRIBUTING.md "Where facts live" — `docs-drift` in CI
   catches renamed knobs and dead links, not a stale sentence). If an install command, repo URL or
   port changed, `data/platforms.json` changed with it — then run `bun run sync-platforms` in
   punktfunk-website and commit, because its download page vendors that file and only refreshes
   when someone does. Same pass for the website itself: does the landing page still describe what
   this release ships (features, platforms, the blog post the CMS expects per release)?
2. **Tag & push.** `git tag -a vX.Y.Z … && git push origin vX.Y.Z` fans out to the build
   workflows. Whichever one wins the create race seeds the release body from this file
   (`scripts/ci/gitea-release.sh` → `ensure_release`, and its PowerShell twin). The release page
   shows the notes immediately.
3. **Wait for green.** Let every platform's CI finish and go green.
4. **Announce.** Dispatch the `announce` workflow (`.gitea/workflows/announce.yml`) with the tag.
   It re-asserts this file over the live release (so any late edit wins) and posts an embed to
   Discord `#releases`. Pressing "go" is the quality gate — a half-built release is never
   announced. Stable-only; a `-rc` tag is refused unless `allow_prerelease=true`.

**If a platform's run never appears, do not re-run the PR run — it cannot publish.** A re-run
replays the original event (`pull_request`), and android's publish steps are gated on a `push`, so
they stay skipped no matter how often you press it. Merging two PRs seconds apart can leave the
older merge sha with **no run at all** — Gitea attributes the window's runs to the newer head
(2026-08-14: `1e5dca4c` lost its run to `b5cace3a`, 12 s later), which is how an android change
reaches main having never been built. Recover it by dispatching `android.yml` on that ref with
**`publish=true`**; that is the only manual path reaching the registry and Play, and a plain
dispatch stays build-only so a stray click can't ship to testers. Check for the gap by matching
your own merge sha in the run list — "CI ran" is not the same as "your commit ran".

Editing the notes after the tag is fine: update this file, then re-run step 4 (or PATCH the body
via the API) — the announce step always re-syncs from the file, so the file stays authoritative
even across a tag re-point.

Canary / `-rc` builds have **no** file here on purpose: they get no curated body and are not
announced.

## Google Play "What's new": `whatsnew/vX.Y.Z.txt`

Play shows its own release notes on the Play Store listing and in the Play Store app, and caps
them at **500 characters per language** — the `vX.Y.Z.md` body is two orders of magnitude too
long, so it gets its own short file: `docs/releases/whatsnew/vX.Y.Z.txt`.

Write it for a **phone/TV user**, not a host operator: only what changed in the Android app is
worth their 500 characters. Plain text (Play renders no markdown), one `•` bullet per line, same
voice rules as below. Copy `whatsnew/TEMPLATE.txt`.

**A `vX.Y.Z` tag without this file fails the android job before it builds.** This is a hard gate,
not a warning, because the failure it prevents is silent: when the file is missing Play does not
show an empty "What's new" — it **carries the previous release's text onto the new version**, so
the store listing describes a build nobody is getting, and nothing surfaces that but reading the
listing. It is the same shape as the v0.22.3 notes announcing a feature that release never
contained. The gate also rejects a file byte-identical to another release's, which is that bug
reached by copy-paste instead of by omission.

The gate runs first in the job, so a miss costs a second and leaves nothing half-published —
no build, no assets on the Gitea release, nothing on Play. Two more checks sit downstream:
`play-upload.py` refuses text over the 500-char cap (printing the real count) before it uploads,
because the API only rejects oversized notes at commit, by which point the AAB is already on Play.

Canary is exempt: it has no curated notes; open-testing users see the previous release's text on
a canary, which is cosmetic and cheaper than gating every main push on a notes file.

Same freeze rule as the notes: once the tag exists, this file is the record of what that
versionCode shipped.

## Voice & format

Voice: `docs/writing.md` §2. Name the thing, then what the reader gets.

**Write for the people who USE Punktfunk to stream their games and desktops — not for the people who
build it.** A non-engineer should finish knowing what's new and whether it affects them.

1. **Lead with the benefit.** Each entry = what the user can now *do*, what now *works*, or what
   stopped *going wrong* — in their words. Implementation is not the story.
2. **No internal vocabulary in the body.** No protocol/message names, code type names, hex codes or
   hardware IDs, crate/component names, or API symbols. Translate any essential detail to plain
   language. Name things users recognize (iPad, Apple Pencil, Steam Deck, Android TV, the Windows
   sign-in screen) — not subsystems.
3. **Discord embed = text before the first `##` = the highlight bullets.** Put 3–8 one-line
   highlights in that lead-in. `scripts/ci/discord-announce.sh` posts everything before the first
   `## `. A later `## Highlights` heading is optional duplication; prefer no heading so Discord
   gets the scan.
4. **Be specific and honest** — no vague "various improvements"; a reader should know exactly what
   changed.
5. **Compatibility line up top, in plain terms:** can they update one side at a time? does their
   existing setup keep working? No version numbers in the lead. Windows host+driver matching is
   not “update one side at a time.”
6. **No protocol / ABI / driver / embedder detail in this file at all.** It goes in the root
   `CHANGELOG.md` (see below), and the notes carry a single short `## For developers` section
   linking there. Nothing else in `vX.Y.Z.md` may use an internal name.
7. **Group as New / Improved / Fixed / Security**, one bullet per fact a user could notice,
   grouped by platform or theme. If something needs the reader to *act*, it belongs in the
   lead-in and in `## Before you update`.
8. **`## Thanks` names every outside author of merged code** and what they built. The work is
   theirs; the notes say so. No bug reporters. No such author, no section.

## The technical half: root `CHANGELOG.md`

**Why it is separate.** A user reads one release and wants prose; an embedder wants to
diff *across* releases and see when the ABI moved, which is a table, not a paragraph.

`CHANGELOG.md` is a **compat card**, not a Keep-a-Changelog diary. Ordinary PRs do not
edit it. The record is the commit (and a `BREAKING CHANGE:` footer when the reader must act).

**Format.** Newest first, one `## vX.Y.Z` card each (or `## Unreleased` until the bump
retitles it). Lead, version table (every row from the previous card, unchanged marked),
**Breaking**, then a short **Knobs** list (env, JNI arity, CLI) for actions that do not
move a version integer. No Added/Changed/Fixed diary. Internal names are the point here —
use them.

**Linking.** The notes link to the file **at the tag**, not at `main`:
`https://git.unom.io/unom/punktfunk/src/tag/vX.Y.Z/CHANGELOG.md`. A release's notes are frozen; a
link to `main` would silently start describing a later release.

**Same freeze rule.** Add the release's card in the version-bump commit, alongside the notes.
While crate version is still the previous tag, the newest card stays `## Unreleased` (short).

The short annotated-**tag** message stays separate and short (a headline + a paragraph); it is the
tag object's message, not this file.
