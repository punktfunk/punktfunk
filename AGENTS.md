# AGENTS.md

Guidance for coding agents working in this repository.

## Writing

`scripts/ci/check-writing.sh` fails the PR. If it fails: shorten. Do not add `writing-ok`
unless the extra lines are a SAFETY/lifetime trap. Rules: `docs/writing.md`.

### Commits

- `type(scope): summary` — imperative, ≤72 characters, no trailing period, no `and`,
  no semicolon. Scope is a subsystem (`host`, `hyprland`, `abr`).
- Body: three short paragraphs max, **200 words**. Why only. No `Co-Authored-By`.
- Measurements and rejected paths go on the PR.
- Gitea PR title is the merge subject. Same shape as the commit.

Bad: `The retry loop stops eating the restore that re-lights the desk`
Good: `fix(host/hyprland): keep topology restore across pipeline retries`

### Changelog

Do not edit `CHANGELOG.md` on ordinary PRs. Commits are the log.
If the reader must act, add a `BREAKING CHANGE:` footer.
Release notes are written at version bump: skill `write-release-notes`
(`.agents/skills/write-release-notes/SKILL.md`), rules in `docs/writing.md` §2.

### Comments

Touch a function → rewrite its comment in the same diff. Do not sweep the file.

Present tense. The live rule, for someone with the file open and not the git log.
Cover the next five lines with your hand: if the comment is only interesting as
history (old versions, a scare, a soak, a ticket), delete it. Keep the invariant.
Not a poem. `// SAFETY:` is a proof, not how the bug was found.

- `//` : four lines (fail at 6). `//!` / `///` : 8–20 (fail at 24). Length is a
  backstop, not the style. A four-line war story is still wrong.
- Keep lifetime, weak-ref, why this number. One to three lines.
- A comment never enforces a trust boundary.

Bad: `A v2 host never stamps the field, so a v3 driver would refuse every attach…`
Good: `A host that leaves this field zero fails the bind.`
Bad: `Field 2026-08-28, iPad Pro / iOS 27 over Tailscale: …`
Good: `250 ms ≈ 30 refreshes at 120 Hz. A miss freezes the picture.`

### Error messages

Two registers, picked by who reads the line. Rules: `docs/writing.md` §4.

- **Operator** (`anyhow` context, `bail!`, `expect`, `#[error]`): a lowercase
  phrase naming the operation. No `failed to` / `could not` prefix, no trailing
  period. `.context("open {path}")`. A `tracing` event takes the noun phrase
  instead — `"hook command did not launch"` — with the cause in a field.
- **User** (console, TUI, tray, setup, client apps, `api_error`): one plain
  sentence of what did not happen, then the next move if there is one. No crate
  or symbol names, no errno.

Both: `Couldn't` / `can't`, never `Could not` / `cannot` / `unable to`. Append
the cause once — ` — ` in prose, `: ` in operator lines. Never a bare code.

## Client settings

Do not add a player-facing setting without the maintainer's yes. First make it automatic, follow
the OS, or fix the detection. Automatic means automatic; never override what the player picked.
Rules and examples: `docs/settings.md`.

## Identifiers

Reverse-DNS IDs are lowercase `io.unom.punktfunk`, with a lowercase suffix. Adding or renaming an
app, bundle, package or service ID: read `docs/identifiers.md` first, for platform conventions and
the IDs that never change.

## Feature requests

New features do not belong on `git.unom.io`. Discuss a feature, a design proposal or a
"which way should this go" question with the maintainer first — no issue, no PR. The tracker
holds bugs and work the maintainer has already agreed to.

## Agent skills

Project SKILL.md files live in `.agents/skills/`. A harness that only
scans its own vendor dir still finds them via this file.

### Issue tracker

Issues live as Gitea issues in `unom/punktfunk` on `git.unom.io`, driven by the `gitea` MCP server
(`gh`/`glab`/`tea` do not work here), and every write needs the user's go-ahead first.
See `docs/agents/issue-tracker.md`.

### Triage labels

The five canonical roles, each label string equal to its name — `needs-triage`, `needs-info`,
`ready-for-agent`, `ready-for-human`, `wontfix` — none of which exist in the tracker yet.
See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `CONTEXT.md` and one `docs/adr/` at the repo root, covering the whole
workspace. See `docs/agents/domain.md`.

## Scroll verification

- `cargo test -p punktfunk-core --features quic scroll` checks wire and compatibility.
- `cargo test -p pf-inject --lib scroll` checks the production host mapping plans on any OS.
- `cargo test -p pf-presenter --no-default-features` runs pure touch and scroll tests on macOS;
  it does not compile the Linux/Windows presenter or injector implementations.
- `swift test --package-path clients/apple --filter ScrollCaptureTests` needs an ABI-matched,
  signed XCFramework. Its packaged header must define `PUNKTFUNK_FEATURE_QUIC`.
- `scroll-probe` (`tools/scroll-probe`), run in a Linux host's session, prints what an app
  receives from injected scroll: source, distance, value120, stops, cadence. `--listen N` only
  listens, for a real client scrolling over its window; the host's
  `RUST_LOG=pf_inject::scroll=trace` logs the same scroll as it came off the wire.
- `scroll-probe --inject-at WxH` opens no window and injects at that desktop's centre, onto a
  real app. `tools/scroll-probe/gtk-scroll-logger.py <log>` is that app for GTK 4: it logs each
  scroll event, each kinetic `decelerate`, and the scrolled window's position per frame. A
  client's lumpy scroll shows there as large deltas tens of milliseconds apart.
- Scope ripwire source reviews with `--exclude=clients/apple/.build`
  `--exclude=clients/apple/PunktfunkCore.xcframework`; downloaded sources and packaged headers
  otherwise pollute the symbol graph and clone findings.

## Console verification

- `cargo test -p pf-console-ui` runs the console's tests on macOS; CPU raster, no GPU.
- `PF_CONSOLE_DUMP=<dir> cargo test -p pf-console-ui --lib -- --ignored dump` writes every
  screen as a PNG.

## Vulkan verification

- `sh scripts/vk-validation-tests.sh` runs the ignored GPU tests of pf-zerocopy, pf-encode,
  pf-vkdecode and the presenter's CSC under the Khronos validation layer, on a box with a device
  and the layer installed. It exits 1 on any validation error and prints each under its test. A
  test that fails for the driver's own reasons does not change the exit code: compare those with
  `main`.
- `PUNKTFUNK_VKDECODE_ARRANGEMENT=sampler` puts any card on the route a device with no plane
  views takes (Pascal on Linux, which we do not own). The script runs pf-vkdecode's tests that way.
- On NVIDIA an H.264 decode faults inside the driver under the layer (615.78), and that ends its
  test binary. Run those tests bare, and the layer with `--skip h264`.
