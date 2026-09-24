---
name: write-release-notes
description: Write Punktfunk stable release notes from git history. Use when cutting a version, writing docs/releases/vX.Y.Z.md, a CHANGELOG.md release card, Play whatsnew, /release-notes, or bumping a stable tag.
---

# Write release notes

Procedure for a stable `vX.Y.Z`. Voice and shape: `docs/writing.md` §2.
Ordinary PRs do not run this and do not edit `CHANGELOG.md`.

## Inputs

1. Previous stable tag (example: `v0.34.0`).
2. Two passes over git, never one. First `git log --no-merges --format='%s' vPrev..HEAD | grep -E '^(feat|fix|perf|security)' | sort` and group the subjects by scope; then read the bodies of each group (`--grep`). Skip `chore` / `ci` / `test` / `docs` unless the body names a user-facing fact. Cluster by **user theme**, not crate. `BREAKING CHANGE:` footers feed Before you update.
   `git log --no-merges --format='%an' vPrev..HEAD | sort | uniq -c` names the contributors for `## Thanks`; list each outside author's commits and say what they built.
3. A `## Unreleased` dump in `CHANGELOG.md`, if one exists, is required reading **after** git log: it carries the "what the reader must do" that pre-footer commits lack. It is not the source. Then replace it with a **short card** (lead + versions + Breaking + knobs). Retitle `## Unreleased` → `## vX.Y.Z` only in the **version-bump** commit. Do not start a new dump. The 0.35 cut may still read the dump: those commits lack `BREAKING CHANGE:` footers.
4. Versioned surfaces from **code**, vs the previous tag (do not invent numbers):
   - Wire: `WIRE_VERSION` in `crates/punktfunk-core/src/lib.rs`
   - C ABI: `ABI_VERSION` / `PUNKTFUNK_ABI_VERSION` in that file and `include/punktfunk_core.h`
   - Driver protocol: `MIN_DRIVER_PROTOCOL_VERSION` in `crates/pf-driver-proto/src/lib.rs`
   - Gamepad channel, plugin schema, OpenAPI (`api/openapi.json` info.version), gamescope `+pfhdrN`, SDK / plugin-kit tags — copy the rows the previous CHANGELOG section already lists; mark unchanged.
5. Knobs, mechanically: `git grep -ohE 'PUNKTFUNK_[A-Z0-9_]+' vPrev -- crates | sort -u` against the same at HEAD; every new name is a Knobs line or a conscious skip. Same for new `punktfunk` / `punktfunk-host` subcommands (`docs-site/content/docs/(reference)/host-cli.md` diff).
6. `git diff --stat vPrev HEAD -- docs-site/content` — every changed page is a user-facing fact; a new page is a bullet.

## Outputs (same bump commit; drafts may exist earlier)

1. `docs/releases/vX.Y.Z.md` — humans, Gitea body, Discord.
   Discord (`scripts/ci/discord-announce.sh`) posts **everything before the first `## `**. Put the 3–8 highlight bullets in that lead-in. A later `## Highlights` heading is optional duplication; prefer no heading so Discord gets the scan.
2. `docs/releases/whatsnew/vX.Y.Z.txt` — Android only, 500 **characters** (`len()`, not `wc -c`), `whatsnew/TEMPLATE.txt`.
3. `CHANGELOG.md` card: lead, version table, Breaking, short **Knobs / embedder** list (env, JNI arity, CLI) for actions that do not move a version integer. No Added/Changed/Fixed diary.
4. Stop. A human reads the lead-in before the tag.

Voice: `docs/writing.md` §2. Name the thing, then what the reader gets. Do not paste `git log`. Do not invent version numbers.

## Coverage, before you stop

The first 0.35 draft dropped Android PyroWave, 10-bit SDR, the Security batch and every contributor. Check:

- Every `feat` / `fix` / `perf` / `security` subject is in the notes, on the card, or a conscious skip. A theme bullet collects facts; it never replaces them.
- Every platform with commits — Android, iPhone/iPad, Apple TV, Mac, Windows client, Linux client, webOS, Steam Deck, browser; Windows host, Linux host per compositor, SteamOS host — has a bullet or a deliberate skip.
- `## Security` and `## Thanks` are filled, or deleted on purpose.

Canary / `-rc`: no file here (existing `docs/releases/README.md` rule).
