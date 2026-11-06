// Reads the shared message files by the format in l10n/SPEC.md: `{name}` placeholders with
// `\{` `\}` `\\` escapes, one plural selector, a message a language lacks read in English.
// Pure: rollup.config.js builds the catalog, i18n.ts picks the language.

/** A plain message, or a plural one: `[{ declarations, selectors, match }]`. */
export type Message = string | [{ declarations: string[]; match: Record<string, string> }];

export interface Bundle {
  source: string; // the language every other one falls back to
  shipped: string[]; // source first
  fallback: Record<string, string>; // `locales.json` `fallback`: one tag read as another
  messages: Record<string, Record<string, Message>>; // language -> key -> message
}

export type Params = Record<string, string | number>;

/** The shipped language best matching `tags` (most preferred first); the source when none does. */
export function pick(tags: readonly string[], shipped: readonly string[], fallback: Record<string, string>): string {
  const find = (tag: string) => shipped.find((l) => l.toLowerCase() === tag.toLowerCase());
  for (const raw of tags) {
    let tag = raw.trim().replace(/_/g, "-");
    for (;;) {
      const via = Object.entries(fallback).find(([from]) => from.toLowerCase() === tag.toLowerCase());
      const hit = find(tag) ?? (via && find(via[1]));
      if (hit) return hit;
      const cut = tag.lastIndexOf("-");
      if (cut < 0) break;
      tag = tag.slice(0, cut);
    }
  }
  return shipped[0];
}

/** `lang`'s text for `key` filled with `params`; English when `lang` lacks it, the key when no one has it. */
export function say(bundle: Bundle, lang: string, key: string, params: Params = {}): string {
  const from = key in (bundle.messages[lang] ?? {}) ? lang : bundle.source;
  const message = bundle.messages[from]?.[key];
  if (message === undefined) return key;
  let text: string;
  if (typeof message === "string") {
    text = message;
  } else {
    const { declarations, match } = message[0];
    const count = declarations.map((d) => /^local \w+ = (\w+): plural$/.exec(d)?.[1]).find(Boolean);
    const category = new Intl.PluralRules(from).select(Number(params[count ?? ""]));
    text = match[`n=${category}`] ?? match["n=other"] ?? match["n=*"] ?? "";
  }
  return text.replace(/\\([{}\\])|\{(\w+)\}/g, (whole, escaped, name) =>
    escaped ?? (name in params ? String(params[name]) : whole),
  );
}
