---
title: Contributing
description: Get a change merged — hooks, the local pass, draft pull requests, CI lanes and labels, generated files and the docs rules.
---

How a change gets from your clone into `main`: the hooks, the local pass, the pull request and the
CI lanes it runs.

Talk a new feature or design through with the maintainer first, for example on
[Discord](https://discord.gg/wzEGg9y45z), before you open an issue or a pull request; the tracker
holds bugs and agreed work. A new
player-facing setting needs the maintainer's yes first
([docs/settings.md](https://git.unom.io/unom/punktfunk/src/branch/main/docs/settings.md)).
Security reports go to **security@punktfunk.com**, never to an issue.

## Git hooks

Enable the repo hooks once per clone:

```sh
git config core.hooksPath scripts/git-hooks
```

On commit they format your staged Rust and JavaScript, re-stage it, and run the writing gate. A
file with both staged and unstaged edits is refused rather than rewritten. On push they check
rustfmt (main and driver workspaces), Biome, the writing gate and unsafe hygiene, and print the
command that fixes each failure. Biome runs only where `web/` or `plugin-kit/` has `node_modules`.

## The full local pass

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Then the checks for what you touched: `web/`, `docs-site/`, a client, or platform-gated code from
another OS. [Testing](/docs/developers/testing) lists them. When your local `main` lags, run the
writing gate against the remote: `WRITING_BASE=origin/main sh scripts/ci/check-writing.sh`.

## Open a pull request

Open it as a draft while you iterate: a `WIP:` title prefix in Gitea. A draft runs the cheap gates
only: writing style, docs drift and links, secret scan, the `bun.nix` check, and the SDK, plugin-kit
and Decky typechecks.

Mark it ready for review to run `rust`, `rust-arm64`, `web` and `docs-site` too, or add the
`ci:all` label to run them on a draft. Neither starts a run by itself: push again, or press
**Re-run** on the latest run in the Actions tab. Each lane runs only when the change touches its
paths. The installer smoke test and the Windows host lint filter by path alone, so they run on
drafts too.

### Platform lanes

These are label-only; being ready for review isn't enough. Only someone with write access can add
a label.

| Label | Runs |
|---|---|
| `ci:android` | The Android build; attaches the debug APK to the run as a zip |
| `ci:apple` | The Apple client build and tests |
| `ci:windows-client` | The Windows client |
| `ci:windows-host` | The Windows drivers and the setup test |
| `ci:nix-rust` | The Nix flake |
| `ci:all` | Everything above, plus the ready-for-review lanes |

The debug APK installs beside a store build instead of replacing it. A push to `main` runs every
`ci.yml` lane; the platform workflows run there when their own paths change.

## Generated files

These are checked in, and CI fails when the committed copy drifts:

| File | Regenerate |
|---|---|
| `include/punktfunk_core.h` | `cargo build -p punktfunk-core` |
| `api/openapi.json` | `cargo run -p punktfunk-host -- openapi > api/openapi.json` |
| `docs-site/public/openapi.json` | `cp api/openapi.json docs-site/public/openapi.json` |
| `sdk/src/gen/punktfunk.ts` | `cd sdk && bun run gen` |
| `docs-site/src/data/platforms.json` | `cp data/platforms.json docs-site/src/data/platforms.json` |
| `THIRD-PARTY-NOTICES.txt` (and the clients' copies) | `bash scripts/gen-third-party-notices.sh` |
| `web/bun.nix`, `sdk/bun.nix` | `sh scripts/ci/check-bun-nix.sh --fix` |
| `*.spv` shaders in `pf-zerocopy` and `pf-encode` | `glslangValidator -V <name>.comp -o <name>.spv` |
| Settings table in `configuration.md` | `UPDATE_SETTINGS_DOCS=1 cargo test -p pf-host-config docs_table_is_current` |

A management-API change needs the three OpenAPI rows. A `Cargo.lock` change needs the notices. A
new dependency must be permissive; see [Licensing](#licensing).

## Docs

Every user-facing fact has one home, and everything else links to it:

| Surface | Holds | Never holds |
|---|---|---|
| [docs-site](https://docs.punktfunk.unom.io) (`docs-site/content/`) | Every user-facing fact: install, config, features, troubleshooting | Design rationale |
| READMEs | Build notes, traps, and links into the docs | User walkthroughs |
| [punktfunk.unom.io](https://punktfunk.unom.io) (separate repo) | Marketing, downloads, blog | Instructions |

When a change moves a user-facing fact, update the page that owns it in the same pull request. How
to write a page: [docs/writing.md §4c](https://git.unom.io/unom/punktfunk/src/branch/main/docs/writing.md#4c-docs-pages).
CI checks the textual half: `check-docs-drift.sh` (the OpenAPI and `platforms.json` snapshots,
`PUNKTFUNK_*` names the docs mention, host commands in [Host CLI](/docs/host-cli)) and
`check-docs-links.sh` (page links, heading anchors, relative file links). A new `PUNKTFUNK_*`
variable needs a docs mention, or the baseline in `scripts/ci/docs-undocumented-env-baseline.txt`
raised in the same commit.

## Writing rules

[docs/writing.md](https://git.unom.io/unom/punktfunk/src/branch/main/docs/writing.md) is the house
style, with a checklist for every pull request:

- commit subjects and bodies (§1): `type(scope): summary`, and the pull-request title is the merge
  subject;
- the changelog (§2): ordinary pull requests don't edit `CHANGELOG.md`; add a
  `BREAKING CHANGE:` footer when the reader must act;
- comments (§3) and error messages (§4).

`scripts/ci/check-writing.sh` enforces the lengths and fails the pull request.

## Licensing

Contributions are licensed MIT OR Apache-2.0 (inbound = outbound), copyleft code is never pasted,
and every dependency must be permissive. The terms:
[CONTRIBUTING.md](https://git.unom.io/unom/punktfunk/src/branch/main/CONTRIBUTING.md#licensing-of-contributions-inbound--outbound).
