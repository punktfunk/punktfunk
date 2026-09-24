---
title: Releasing
description: For maintainers — how canary and stable builds ship, how to cut a stable release, and how the release notes are written.
---

How builds reach users, and the steps a maintainer takes to cut a stable release. Subscribing a box
to a channel is [Release channels](/docs/channels).

## Canary and stable

| | Canary | Stable |
|---|---|---|
| Built by | A push to `main` that touches a platform's paths | A `vX.Y.Z` tag, every platform |
| Version | One minor ahead of the latest stable tag, plus a per-channel build suffix | The tag |
| Published to | Each platform's canary channel | Each stable channel, plus one [Gitea release](https://git.unom.io/unom/punktfunk/releases) with every artifact |
| Android | Play open testing (`beta`) and closed testing (`alpha`) | Play production at 100% |
| Apple | TestFlight | TestFlight; the App Store after you submit it |
| Host update check | Canary manifest, after each Windows host canary | Stable manifest, published by `announce` |

`scripts/ci/pf-version.sh` (and its PowerShell twin `pf-version.ps1`) derives every version, so
nothing is bumped by hand to move canary: cutting `v0.40.0` moves the next canary to `0.41.0`.
Where a store needs a numeric version (MSIX, the App Store), a `-rc` or `+meta` suffix is dropped.
The Linux packages hand the version to the build as `PUNKTFUNK_BUILD_VERSION`, which `--version`
reports; the Windows packer stamps it into the finished binary.

## Cut a stable release

1. **Prepare the cut on a branch** (`release/vX.Y.Z`). One commit, `chore(release): cut X.Y.Z`,
   carries:
   - the `version` in the root `Cargo.toml`, with both lockfiles refreshed;
   - both OpenAPI copies, whose `info.version` follows the crate version;
   - `docs/releases/vX.Y.Z.md`, `docs/releases/whatsnew/vX.Y.Z.txt` and the `CHANGELOG.md` card
     ([Release notes](#release-notes)).

   ```sh
   cargo metadata --offline --format-version 1 >/dev/null
   cargo metadata --offline --format-version 1 \
     --manifest-path packaging/windows/drivers/Cargo.toml >/dev/null
   cargo run -p punktfunk-host --locked -- openapi > api/openapi.json
   cp api/openapi.json docs-site/public/openapi.json
   ```

   If the lockfile moved a dependency, also run `bash scripts/gen-third-party-notices.sh`. With the
   diff open, check that every user-facing change updated its docs page; if `data/platforms.json`
   changed, run `bun run sync-platforms` in the website repo too.
2. **Merge it once `main` is green.** Before merging, sweep what landed on `main` since you drafted
   the notes; a late C ABI or API change moves the card.
3. **Tag the merge commit** with an annotated tag (a headline, a user paragraph, an embedder
   paragraph) and push it:

   ```sh
   git tag -a vX.Y.Z <merge-commit>
   git push origin vX.Y.Z
   ```

4. **Wait for every platform to go green.** Each workflow builds at `X.Y.Z`, publishes to its
   stable channel and attaches its artifact to the one Gitea release, which takes its body from
   `docs/releases/vX.Y.Z.md`.
5. **Announce.** Dispatch the `announce` workflow from `main`, so a notes fix made after the tag is
   the one it posts, with `tag` and `authenticode_sha256`. Copy the hash from the Windows host tag
   run, step **Report stable Authenticode identity** (`AUTHENTICODE LEAF SHA-256 FOR ANNOUNCE`).
   `announce` publishes the signed stable update manifest, which is when hosts learn about the
   release, then posts the notes to Discord.
6. **Promote the stores.** Android reaches Play production by itself. To ramp, halt or roll back,
   use the Play Console or `android-promote.yml` (a dry run unless you turn `dry_run` off). Apple:
   submit the TestFlight build for review in App Store Connect.

**An `-rc` tag publishes too.** It builds like a stable tag, Android to Play production included,
and needs its own `docs/releases/whatsnew/<tag>.txt`. `announce` refuses it unless
`allow_prerelease` is set, and never puts it in the update feed.

To fix a tag after pushing it, land the fix on `main`, then move the tag with
`git tag -f -a vX.Y.Z <commit>` and `git push -f origin refs/tags/vX.Y.Z`. The new tag runs cancel
the old ones.

## Release notes

Write them in the cut commit, before the tag, with the `write-release-notes` skill
([.agents/skills/write-release-notes/SKILL.md](https://git.unom.io/unom/punktfunk/src/branch/main/.agents/skills/write-release-notes/SKILL.md)).
It reads `git log` since the previous tag. Voice and shape:
[docs/writing.md §2](https://git.unom.io/unom/punktfunk/src/branch/main/docs/writing.md#2-changelogs).

| File | Reader | Rule |
|---|---|---|
| `docs/releases/vX.Y.Z.md` | People who stream | Becomes the Gitea release body. Everything before the first `##` is the Discord post: 3–8 highlights. |
| `docs/releases/whatsnew/vX.Y.Z.txt` | Play Store | 500 characters at most. The Android tag build fails without it, or with a copy of another release's file. |
| `CHANGELOG.md` card | Embedders, packagers | Version table, Breaking, Knobs. Links to it use the tag, not `main`. |

Ordinary pull requests don't touch `CHANGELOG.md`. A change the reader must act on carries a
`BREAKING CHANGE:` footer, which the skill turns into **Before you update**. To edit the notes after
the tag, change the file on `main` and dispatch `announce` again (it posts to Discord again), or
patch the release body through the Gitea API. Flow and voice in full:
[docs/releases/README.md](https://git.unom.io/unom/punktfunk/src/branch/main/docs/releases/README.md).

## A platform's run is missing

Two merges seconds apart can leave the older commit with no run, and re-running a pull-request run
can't publish. Dispatch the workflow on that ref instead: `android.yml` and `windows-host.yml`
publish only with `publish=true`; a plain dispatch just builds.
