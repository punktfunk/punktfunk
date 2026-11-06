# The message format

`pf-l10n`, Paraglide and unom translate read the same files. This is the part of the inlang
message format all three agree on. The build and `pf-l10n check` enforce it, and
`vectors.json` pins what a reader must produce from it.

## Files

- `messages/<part>/<lang>.json` is one JSON object per language. `<lang>` is a BCP-47 tag:
  `de`, `pt-BR`, `zh-Hant`. The source language (`locales.json`, `en`) has a file in every part.
- A key appears once per file. A `$schema` key is ignored.
- A key is `<area>_<thing>[_<role>]`: lowercase letters and digits, words joined by single
  underscores, two words at least. It is unique across all parts, `web` included.
- `messages/<part>/notes.json` maps a key to one line for the translator. Every message with
  a placeholder has one.

## A plain message

A string of text, placeholders and escapes.

| In the file (JSON) | The message reads |
|---|---|
| `"Statistics size"` | Statistics size |
| `"{keys} cycles it during a stream."` | the value of `keys`, then the text |
| `"Write \\{name\\} or \\\\"` | Write {name} or \ |

- `{name}` is a placeholder. A name is lowercase snake_case and not a Rust or Swift keyword
  (`{type}` is refused: name it after what it holds).
- `\{`, `\}` and `\\` are a literal brace and backslash. JSON doubles the backslash.
- Refused: an empty string; a positional `{0}`; a function `{count :number}`; markup `{#b}`;
  a `{` without its `}`, or a `}` alone; a backslash before anything else.

## A plural message

A count is a plural message, in exactly this shape:

```json
"preset_pinned": [{
  "declarations": ["input count", "local n = count: plural"],
  "selectors": ["n"],
  "match": {
    "n=one": "Pinned to {count} host",
    "n=other": "Pinned to {count} hosts"
  }
}]
```

- An array of one object with the fields `declarations`, `selectors` and `match`, no others.
- `declarations`: any number of `input <name>`, and one `local <n> = <input>: plural`. That
  input is the count.
- `selectors`: exactly `["<n>"]`. One count per message: a sentence with two counts is two
  messages.
- `match`: keys `<n>=zero`, `one`, `two`, `few`, `many`, `other`, or `<n>=*`, which means
  `other`. Each category once. Each value is a plain message as above. `{n}` in a value is
  refused; the count is `{count}`, its input.
- A language's plural has exactly the categories CLDR gives it:

| Languages | Categories |
|---|---|
| en, de, nl, sv, da, nb, fi, tr | one, other |
| es, fr, it, pt | one, many, other |
| pl, cs, ru, uk | one, few, many, other |

A language without a rule in `src/plural.rs` cannot ship.

## TypeScript-only parts

The parts `rules.json` lists under `typescript_only` (`web`, `browser`, `decky`) are read by
Paraglide alone. `check` holds them to valid JSON, the key rules and matching placeholders;
markup, several selectors and functions are Paraglide's to read.

## Reading a message

- **Arguments** follow the order the placeholders first appear in the English text, a plural's
  `other` variant read first. A count that no variant shows comes first. This order is the
  Rust function's parameters, Android's `%1$s`, and the Swift accessor's parameters.
- **Fallback.** A message the language lacks reads English. A plural without the count's
  category reads `other`.
- **The language** is picked from the OS's list, most preferred first. Each tag is tried as
  it is, then through `locales.json`'s `fallback`, then one subtag shorter (`de-AT`, `de`).
  Case and `_` do not matter. Nothing matches: the source language.
