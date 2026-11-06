import { readdirSync, readFileSync } from "node:fs";
import deckyPlugin from "@decky/rollup";

const l10n = new URL("../../l10n/", import.meta.url);
const read = (path) => {
  try {
    return JSON.parse(readFileSync(new URL(path, l10n), "utf8"));
  } catch {
    return {}; // a part with no file in this language
  }
};

// The source of every `.ts`/`.tsx` file, to keep only the messages the plugin names.
const sources = (dir = new URL("./src/", import.meta.url)) =>
  readdirSync(dir, { withFileTypes: true }).flatMap((e) =>
    e.isDirectory()
      ? sources(new URL(`${e.name}/`, dir))
      : /\.tsx?$/.test(e.name)
        ? [readFileSync(new URL(e.name, dir), "utf8")]
        : [],
  );

/** `virtual:l10n`: the shipped languages' common, client and decky messages the code names,
 *  and the host's `reject_` messages, which the code names by token. See src/i18n.ts. */
function messages() {
  const id = "\0virtual:l10n";
  return {
    name: "l10n",
    resolveId: (source) => (source === "virtual:l10n" ? id : null),
    load(file) {
      if (file !== id) return null;
      const { source, shipped, fallback } = read("locales.json");
      const code = sources().join("\n");
      const used = (key, part) => part === "host" ? key.startsWith("reject_") : code.includes(`"${key}"`);
      const languages = [source, ...shipped.filter((l) => l !== source)];
      const catalog = Object.fromEntries(
        languages.map((lang) => [
          lang,
          Object.fromEntries(
            ["common", "client", "decky", "host"].flatMap((part) =>
              Object.entries(read(`messages/${part}/${lang}.json`)).filter(([key]) => used(key, part)),
            ),
          ),
        ]),
      );
      return `export default ${JSON.stringify({ source, shipped: languages, fallback, messages: catalog })};`;
    },
  };
}

export default deckyPlugin({ plugins: [messages()] });
