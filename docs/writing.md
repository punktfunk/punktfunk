# Punktfunk writing standards

House style for **commits**, **changelogs**, and **comments**.
`scripts/ci/check-writing.sh` fails a PR that breaks the caps below.

## 0. Where facts go

| Fact | Lives in |
| --- | --- |
| What changed, in one greppable line | Commit **subject** |
| Why it changed | Commit **body** (PR if it needs a diagram) |
| Reader must act (API, package, driver, flag) | Commit footer `BREAKING CHANGE: <action>` |
| What a user can do now | `docs/releases/vX.Y.Z.md` (written at bump, from git log) |
| Did wire / ABI / driver proto move | `CHANGELOG.md` version table for that tag |
| New env / JNI / CLI that does not move a version integer | `CHANGELOG.md` **Knobs** on that card (and a `BREAKING CHANGE:` footer if the reader must act) |
| Investigation, measurements, rejected paths | Pull request and `docs/adr/` |
| Invariant that must remain true | Comment, type, or test |

---

## 1. Commits

```
type(scope): imperative summary

Why it failed for a user. What was actually wrong.
What you changed. Wrap at 72.

Fixes #123
```

If an embedder, packager, or user must act:

    BREAKING CHANGE: install matching host and driver (protocol 8).

The release-notes skill greps this footer for `## Before you update`.
Do not also add a `CHANGELOG.md` bullet on that PR.

- **50 characters** aim. **72 hard cap.** No trailing period.
- Imperative, present tense: `keep`, `skip`, `advertise`.
- One logical change. A subject with “and” or a semicolon is two commits.
- Body: at most three short paragraphs, **200 words**.
- Scope is a subsystem a newcomer greps: `host`, `hyprland`, `mdns`, `abr`, `console`,
  `gamestream`, `android`, `web`, `core`.
- No `Co-Authored-By`. Gitea 1.27 shows the trailer as a second committer. Credit in prose.
- Field logs, SKUs, soak minutes, rejected paths, RFC numbers: **PR body**, not the commit.

CI checks every commit on the PR: missing `type(scope):`, subject starting `The …`,
subject over 72 characters, body over 200 words, or a `Co-Authored-By` trailer fails the job.

| Type | Use |
| --- | --- |
| `feat` | User-visible capability that did not exist |
| `fix` | A bug. Put the symptom in the body |
| `docs` | Docs, comments-as-docs, release notes. No behaviour change |
| `refactor` | Same behaviour, different shape |
| `perf` | Same behaviour, cheaper. Name the metric if you have one |
| `test` | Tests only |
| `chore` | Deps, version bumps, generated files |
| `ci` | Pipelines and gates |
| `security` | Trust-boundary changes |

Bad: `The retry loop stops eating the restore that re-lights the desk`
Good: `fix(host/hyprland): keep topology restore across pipeline retries`

The Gitea PR title is the merge subject. Write it as the conventional subject.

---

## 2. Changelogs

Voice and shape below. Procedure — git log, when to retitle `## Unreleased`,
who runs the skill — is `.agents/skills/write-release-notes/SKILL.md`.

### `docs/releases/vX.Y.Z.md` — people who stream

Shape:

1. Compatibility line + **3–8 highlight bullets BEFORE the first `##`**, plus one short
   paragraph if the release needs a warning. Discord (`scripts/ci/discord-announce.sh`)
   posts that prefix (1800 chars).
2. `## Before you update` — delete if nothing. Windows host+driver matching is not
   “update one side at a time.”
3. `## New` / `## Improved` / `## Fixed` / `## Security` — one bullet per fact a user could
   notice, grouped by platform or theme. A group heading collects bullets; it never replaces
   them.
4. `## Thanks` — every contributor outside the team, by name, with what they built.
5. `## For developers` — CHANGELOG at the tag + `git log vPrev..vThis`.

Voice: Name the thing, then what the reader gets. No metaphor, origin story, lab, SKU,
soak, “we watched”. A fact about the build with no reader in it is not a bullet. No crate names, protocol hex, or API symbols on the user page.

Bad: `The headline is a control surface for everyone who streams to a screen they hold.`
Bad: `**No bundled FFmpeg.** The host encodes with your GPU's own encoder.`
Good: `**Quick-action ring.** A two-finger twist on the stream opens six buttons.`
Good: `**The packages carry no FFmpeg.** Fedora needs no RPM Fusion.`

### `CHANGELOG.md` — embedders, packagers, plugin authors

A **compat card**, not a diary. Ordinary PRs do not edit it. Newest first.

```markdown
## v0.35.0

N commits since v0.34.0. Wire stays 2. **Driver protocol floor 8.**
Deep dive: `git log v0.34.0..v0.35.0`

### Versions
(table, every row from previous card, unchanged marked)

### Breaking
- **Noun.** What changed. What the reader must do.

### Knobs
env/JNI/CLI that do not move a version integer — name + action
```

No Added/Changed/Fixed diary. Do not rewrite older sections.

Target: two screens. No CI for changelog length.

---

## 3. Comments

A comment is the non-local reason: the trap, the lifetime, the unit on a magic number.
Names and types are the what. If the next five lines already say it, delete the comment.

### Voice

Present tense. The file as it is, for someone who has the code and not the git log.

Cover the next five lines with your hand. If the comment is only interesting as
history — how v2 scared us, a soak, a review ticket, a “this used to” — it belongs
on the PR or in `docs/adr/`. Keep the rule that is still true.

A comment is not a poem. No cadence, no punchline, no plot. Name the invariant
and stop.

A constant’s comment is why the number, not the incident that produced it.
A version comment is the live handshake, not a biography of every bump.

Bad (archaeology): `A v2 host never stamps the field, so a v3 driver would refuse
every attach — lockstep by the handshake, as ever. On-glass 2026-07-23…`
Good (the live rule): `A host that leaves this field zero fails the bind.`

Bad (field report): `Field 2026-08-28, iPad Pro / iOS 27 over Tailscale: …`
Good: `250 ms ≈ 30 refreshes at 120 Hz. A miss freezes the picture.`
Good (commit body / PR): the iPad, the log lines, the reconnect.

`// SAFETY:` is a proof, not how the bug was found. Dates, SKUs, and soak minutes
go stale; prefer a name (`stash_topology_restore_first_wins`) over a twelve-line
comment.

CI length caps are a backstop, not the style. A four-line war story is still wrong.

### Caps

- `//` : at most four lines (CI fails at six).
- `//!` / `///` module map: what it is, the contract, how to pin it, where evidence lives.
  8–20 lines (CI fails at 24).
- Swift has no `//!`, so a `.swift` file's OPENING `//` block is its module map and gets the
  same budget. Every comment below the header is on the `//` cap.
- Keep `// SAFETY:` and FFI/lifetime proofs exact.
- A comment never enforces a trust boundary — a type, a test or an assertion does.

CI counts comments this diff opened (the comment itself, or the comment above an item
whose body changed), in `.rs` and `.swift` alike. If it fails: shorten. Do not add `writing-ok` unless the extra
lines are a SAFETY/lifetime trap.

### When you touch a function, rewrite its comment

Do not sweep the file. Do not open a comments-only PR.

1. Restates the next five lines → delete.
2. Archaeology, field report, lab nickname (`ponytail:`), second copy of the commit body → delete.
3. Lifetime / weak-ref / generation-vs-session / why this number → keep, one to three lines.

### Module rustdoc

8–20 lines: what it is, the public contract, how to pin it, where evidence lives.
Point at `design/` / `docs/adr/`. Do not paste the investigation into `//!`.

### SAFETY

Keep proofs exact:

```rust
// SAFETY: the clipboard is open (the `Clip` guard); the handle returned is
// BORROWED from the clipboard and stays valid while it is open, so it is
// never freed here.
```

---

## 4. Error messages

Two registers. Pick by who reads the line, never by which language you are in.

### Operator register

`anyhow` context, `bail!`, `expect`, `panic!`, `tracing::error!` / `warn!`,
`#[error(…)]`.

A lowercase noun or verb phrase naming the operation that did not happen.
No `failed to` / `could not` / `unable to` prefix — the surface already frames
it as a failure and the chain then says so twice. No trailing period. API,
type and env-var names keep their own case.

```rust
.context("open {path}")?;
bail!("adapter exposes no {kind} decode profile");
```

Bad: `.context("Failed to open the config file")` — framing, no subject.

A `tracing` event is read on its own line, not appended to a chain, so it takes
the noun phrase instead of the bare operation: `"hook command did not launch"`,
`"client log upload failed"`, `"launch rejected"`. Put the cause in a field
(`error = %e`), not in the message.

Bad: `tracing::error!(error = %e, "launch the hook command")` — reads as an
instruction in the log.

### User register

The web console, the TUI, the tray, the setup wizard, every client app, and
every `api_error` string a client puts on screen.

Sentence case. One sentence saying what did not happen, in the words of
someone who streams games — no crate, symbol, protocol, hex code or errno
(`docs/releases/README.md` voice). Then, only when the reader can act, one
more sentence naming the move. No trailing period on a lone first sentence.

```
Couldn't reach the host — it may be asleep.
Check its power settings, then try again.
```

Bad: `Error: mgmt API request failed (os error 61)`

### Both registers

- Append the cause once, after ` — ` in prose or `: ` in the operator
  register. Never both, never twice.
- `Couldn't`, `can't`. Not `Could not`, `cannot`, `unable to`, `failed to`.
- Name the subject the reader knows — `the client key`, not `SecItemAdd`.
- Never a bare code, errno or enum discriminant with no words around it.
- No apology, no exclamation mark, no `Oops`, no `Please`.

A message that only a maintainer can act on is operator register, whatever
window it renders in.

---

## 4b. UI copy

Every string an operator reads *while working a control* — the web console
first, and any surface that grows the same shape. Error messages stay §4.

A control is made clear by its options, not by a paragraph under it.
`web/tools/check-i18n.mjs` fails the build on a string over budget.

| Kind | Budget | Rule |
|---|---|---|
| Label | ≤ 3 words | Names the thing, not the mechanism. `Your monitors while streaming`, not `Topology`. |
| Option | — | The outcome, as the operator would say it: `Turn off`, `Stay on`, `Hand over`. |
| Hint | 1 sentence, ≤ 110 chars | Only when the options cannot carry it. States the consequence or the next move. |
| Warning | 2 sentences | What happens, then what to do. |
| Anything longer | — | A `Docs ↗` link into `docs-site/` beside the hint. The console does not host manuals. |

- Never the implementation: no `atiadlxx.dll`, no `PUT /display/settings`, no
  connector enum in a hint.
- Never a platform in the words — `Windows only` means the control should not
  have been rendered. Gate it on what the host says it enforces.
- Status words are outcomes: `Streaming`, `Kept`, `Kept until released`, `Off`.
  Not `Lingering`, `Pinned`, `Active`.
- Translations get 20% more room; the base locale is where the budget bites.
- The lint's allowlist is for legal and security texts, where the exact wording
  is the point. Each entry is a debt line in `design/web-console-overhaul.md` §2.2.

Bad: `Windows only, takes effect with Exclusive topology. Before disabling your
physical monitors, the host also tells them to power their panel off over the
DDC/CI monitor-control channel, and wakes them again when the stream ends. …`
Good: `Stops the stutter some setups get with a dark monitor. If one stays dark,
press its power button.` + `Docs ↗`

---

## 4c. Docs pages

`docs-site/content/docs/`. Three tabs, three readers:

| Tab | Reader | Shape |
|---|---|---|
| Guide | Sets up a host or streams. Knows no Linux. | One task per page. Numbered steps, happy path first. |
| Reference | Looks up one setting, flag, port or platform. | Tables. Dense is fine. |
| Developers | Changes or extends Punktfunk. | Commands that work from a fresh clone. |

- Open with one sentence: what the reader gets from this page. Never `This page describes`.
- Say what to do. Add why only when skipping the step breaks something, in one clause.
- No history: no `now`, `used to`, `as of 0.38`, `new in`. No incident, lab box or plan doc.
- One home per fact. Link to it; do not restate a command, port, default or table.
  Install lines and ports come from `<Install>` and `<Ports>`.
- Name UI exactly as it renders, in bold: **Pairing** → **Approve**.
- Platform differences go in a table or `<Tabs>`, not a paragraph per platform.
- A troubleshooting entry is `### <symptom as the reader sees it>`, one line of cause, the fix.
- `<Callout>` only for a step that breaks something when missed. Two per page at most.
- A Guide page past 200 lines is two tasks, or it holds reference detail. Split or move it.
- Every command, flag, env var, path, default and label matches the code at HEAD.
- Headings are anchors. The console, setup and host logs link them — `check-docs-links.sh`
  fails a link to a heading that is gone. Rename one, fix its links in the same diff.
- The settings table in `configuration.md` is generated: `UPDATE_SETTINGS_DOCS=1 cargo test
  -p pf-host-config docs_table_is_current`.

Bad: `The repo is public and signed; this adds it and installs the host together with the
console. They are named in the line rather than left to apt: punktfunk-host only recommends
them, so a box with APT::Install-Recommends "0" would end up with a host you cannot pair with.`
Good: `Add the repo and install the host, the console and the plugin runner:`

---

## 5. Checklist (every PR)

- [ ] Subject is `type(scope): summary`, ≤ 72 characters, imperative, no period
- [ ] Subject names a subsystem a newcomer would grep
- [ ] Body is why, not the investigation (investigation is on the PR)
- [ ] One logical change; no “and” holding two fixes together
- [ ] feat/fix/security/perf has a body
- [ ] BREAKING CHANGE footer if the reader must act
- [ ] This PR did not add a CHANGELOG.md bullet
- [ ] User-facing fact updated on the docs-site page (not docs/releases/)
- [ ] New comments state an invariant or a trap, not a recap of the diff
- [ ] Comments are present-tense live rules, not archaeology or a poem
- [ ] Module rustdoc still fits on one screen (CI fails a touched `//!` / `///` at 24 lines)
- [ ] Touched `//` blocks are at most four lines (CI fails at six), except SAFETY proofs
- [ ] No new comment that is the only enforcement of a trust boundary
- [ ] New error messages pick a register: operator lines are lowercase phrases,
      user lines are one plain sentence with the next move
- [ ] No `failed to` / `could not` / `unable to` framing, and no doubled cause
- [ ] `scripts/ci/check-writing.sh` is green

---

## 6. Adoption

Do not rewrite old `CHANGELOG.md` sections or old `docs/releases/v*.md` bodies.
Do not sweep existing module rustdoc. New notes follow this file. Rewrite a comment
when you already open that function.

This document does not replace `docs/releases/README.md`.
