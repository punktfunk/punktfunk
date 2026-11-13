# l10n

Every message a person reads in Punktfunk is written once, here. Each platform's build turns
these files into its own form: Rust functions, Android resources, an Apple String Catalog,
Paraglide for TypeScript. No generated platform file is committed. `SPEC.md` is the format.

## Add a message

1. Pick the part: the widest one where the text is true word for word and stands for the same
   thing.
2. Pick a key, `<area>_<thing>[_<role>]`. Roles: `_hint`, `_title`, `_body`, `_confirm`,
   `_failed`, `_a11y`; a bare key is a label. Search every part, `web` included: a key is
   unique across all of them.
3. Add it to `messages/<part>/en.json`. A count is a plural message (`SPEC.md`).
4. A message with a placeholder gets a note in `messages/<part>/notes.json`: where it shows,
   what each placeholder holds.
5. Run `cargo run -p pf-l10n -- check`.
6. Use it:

| Where | Call |
|---|---|
| Rust | `pf_l10n::client::player_stats_scale_pct()`, with the part's feature on `pf-l10n` |
| Kotlin | `stringResource(R.string.key, a)`, `pluralStringResource(R.plurals.key, n, n)` |
| Swift | `L10n.playerStatsScalePct`, `L10n.prefsAdvancedChanged(n)` |
| TypeScript | `m.key()` |

Arguments go in the order the English text shows them. Text is never an identity: do not
compare, parse or key a map on a message.

## Parts

| Part | Holds |
|---|---|
| `common` | Text with no context: a button's verb, a unit, a product term |
| `client` | What every client says: player settings, pairing, hosts, the stream overlay |
| `console` | The controller interface, and the own text of the TV and console clients |
| `desktop` | The GTK and WinUI shells |
| `apple`, `android` | Text only that platform has |
| `browser`, `decky` | The browser client's shell and the Decky plugin (TypeScript-only) |
| `host` | What the host sends as an id: reject reasons, API errors, diagnostics |
| `tools` | Setup, the Windows wizard, the tray, the updater |
| `web` | The web console (TypeScript-only) |

A new Rust part needs a feature of its name in `Cargo.toml`.

## Files

| File | Holds |
|---|---|
| `locales.json` | The source language, the shipped ones, and fallbacks between them |
| `rules.json` | Which parts are TypeScript-only or read by Apple, length budgets by role (`budgets.parts`: a part's own), banned patterns, retired terms, keys allowed over budget or holding Markdown (`debt`) |
| `glossary.json` | Agreed terms, and names never translated |
| `vectors.json` | What a reader of the format must produce; the tests replay it |

A language ships when `locales.json` lists it. Until then its files are drafts: `check`
checks them, the build ignores them. `locales` in `web/project.inlang/settings.json` is the
same list, the source first; `check` fails when the two differ.

## Commands

```sh
cargo run -p pf-l10n -- check              # one line per problem, exit 1 on any
cargo run -p pf-l10n -- check --missing    # also untranslated keys, which never fail
cargo run -p pf-l10n -- check --unused     # also keys no tracked source names, never failing
cargo run -p pf-l10n -- android --out <res dir> --parts common,client,android
cargo run -p pf-l10n -- apple --out <dir> --parts common,client,apple
cargo test -p pf-l10n --all-features
UPDATE_GOLDEN=1 cargo test -p pf-l10n --all-features   # accept new generator output
```

## The Rust runtime

- `pf_l10n::set_languages(&pf_l10n::os_languages())` once at start (`os` feature); without it
  every message is English. Consoles and TVs pass the system's language instead.
- `pf_l10n::get(key)` finds a message where the key is data, such as a host error code.
- `pf_l10n::with_language("de", || …)` reads one language on the current thread, for tests.
- The `pseudo` feature adds `en-XA`: accented, a third longer, in brackets. Text that stays
  plain English under it does not come from here.
