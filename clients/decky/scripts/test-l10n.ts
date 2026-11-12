// Replays l10n/vectors.json through the plugin's message reader, and checks that every message
// key the code names exists, that its placeholders are filled, and that every host refusal has
// words. Run by `pnpm test`.
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { type Bundle, type Message, pick, say } from "../src/l10n.ts";

const json = (path: string) => JSON.parse(readFileSync(new URL(path, import.meta.url), "utf8"));
const l10n = "../../../l10n/";

interface Vectors {
  messages: Record<string, Record<string, Message>>;
  format: { key: string; lang: string; args: Record<string, string | number>; want: string }[];
  pick: { os: string[]; shipped: string[]; fallback: Record<string, string>; want: string }[];
}
const vectors: Vectors = json(`${l10n}vectors.json`);
const bundle: Bundle = {
  source: "en",
  shipped: Object.keys(vectors.messages),
  fallback: {},
  messages: vectors.messages,
};
for (const v of vectors.format) {
  assert.equal(say(bundle, v.lang, v.key, v.args), v.want, `${v.key} in ${v.lang}`);
}
for (const v of vectors.pick) {
  assert.equal(pick(v.os, v.shipped, v.fallback), v.want, `${v.os} of ${v.shipped}`);
}
console.log(`l10n-vectors: ${vectors.format.length} messages, ${vectors.pick.length} picks match`);

// The plugin's own messages, in English: every key the code names resolves, and every
// placeholder of a message is one the call passes.
const parts = ["common", "client", "decky", "host"];
const en: Record<string, Message> = Object.assign(
  {},
  ...parts.map((p) => json(`${l10n}messages/${p}/en.json`)),
);
const src = new URL("../src/", import.meta.url);
const calls: { file: string; key: string; args: string }[] = [];
const sources = readdirSync(src, { recursive: true, encoding: "utf8" });
for (const file of sources.filter((f) => /\.tsx?$/.test(f) && f !== "i18n.ts")) {
  const code = readFileSync(new URL(file, src), "utf8");
  for (const m of code.matchAll(/\bt\(\s*"([a-z0-9_]+)"(?:,\s*\{([^}]*)\})?/g)) {
    calls.push({ file, key: m[1], args: m[2] ?? "" });
  }
}
assert.ok(calls.length > 100, `found only ${calls.length} t() calls`);
const names = (m: Message) =>
  [...JSON.stringify(m).matchAll(/\{([a-z0-9_]+)\}/g)].map((x) => x[1]).filter((n) => n !== "n");
for (const { file, key, args } of calls) {
  assert.ok(key in en, `${file}: t("${key}") has no message`);
  const passed = args.split(",").map((a) => a.split(":")[0].trim()).filter(Boolean);
  for (const n of new Set(names(en[key]))) {
    assert.ok(passed.includes(n), `${file}: t("${key}") does not pass {${n}}`);
  }
}
console.log(`l10n-keys: ${calls.length} t() calls name a message and pass its placeholders`);

// Every plugin message renders in every language file, shipped or draft: no placeholder is left
// open, and a plural reads a form for each count.
const dir = new URL(`${l10n}messages/decky/`, import.meta.url);
const drafts = readdirSync(dir)
  .filter((f) => f.endsWith(".json") && f !== "notes.json")
  .map((f) => f.slice(0, -5));
const all: Bundle = {
  source: "en",
  shipped: drafts,
  fallback: {},
  messages: Object.fromEntries(drafts.map((l) => [l, json(`${l10n}messages/decky/${l}.json`)])),
};
for (const lang of drafts) {
  for (const [key, message] of Object.entries(all.messages[lang])) {
    const params = Object.fromEntries(names(message).map((n) => [n, n === "count" ? 2 : "x"]));
    for (const count of [0, 1, 2, 5, 22]) {
      const got = say(all, lang, key, { ...params, count });
      assert.ok(got && !/(?<!\\)\{\w+\}/.test(got), `${lang}/${key} reads "${got}"`);
    }
  }
}
console.log(`l10n-languages: ${drafts.join(", ")} render`);

// Every reason a host can refuse with has words in the host part: the CLI prints its token.
const reject = readFileSync(
  new URL("../../../crates/core/punktfunk-core/src/reject.rs", import.meta.url),
  "utf8",
);
const tokens = [...reject.matchAll(/= \w+_CLOSE_CODE,\s*"([a-z-]+)"/g)].map((m) => m[1]);
assert.ok(tokens.length >= 18, `found ${tokens.length} reject tokens`);
for (const token of tokens) {
  const key = `reject_${token.replace(/-/g, "_")}`;
  assert.ok(key in en, `${key} (token ${token}) has no message`);
}
console.log(`reject-tokens: ${tokens.length} tokens have words`);
