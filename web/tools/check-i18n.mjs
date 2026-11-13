// Guard: Paraglide compiled every message the source files hold.
//
// A plugin that fails to import is only a warning to the inlang SDK: `paraglide-js compile`
// then exits 0 with no messages, and the console dies at SSR on the first `m.foo()`. A
// message file the build cannot reach is skipped the same way. So this counts the output.
// Budgets, parity and key rules are `cargo run -p pf-l10n -- check` (l10n/rules.json).
//
//   node tools/check-i18n.mjs          # run from web/, after `paraglide-js compile`
//
// Env knobs: PARAGLIDE_OUTDIR (default ./src/paraglide), INLANG_PROJECT (default
// ./project.inlang).

import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";

const outdir = resolve(process.env.PARAGLIDE_OUTDIR ?? "./src/paraglide");
const project = resolve(process.env.INLANG_PROJECT ?? "./project.inlang");

const fail = (msg) => {
	console.error(`ERROR: i18n check failed — ${msg}`);
	process.exit(1);
};

const settingsPath = join(project, "settings.json");
if (!existsSync(settingsPath)) fail(`no inlang settings at ${settingsPath}`);
const settings = JSON.parse(readFileSync(settingsPath, "utf8"));

// A remote module needs the network at compile time, so an offline build (Nix, air-gapped
// CI) compiles zero messages.
for (const mod of settings.modules ?? []) {
	if (/^https?:/i.test(mod)) {
		fail(
			`inlang module "${mod}" is a remote URL — vendor it as a devDependency and reference it by path, ` +
				`else offline builds compile zero messages`,
		);
	}
	const modPath = resolve(project, "..", mod);
	if (!existsSync(modPath))
		fail(
			`inlang module "${mod}" does not exist at ${modPath} (run \`bun install\`?)`,
		);
}

// Every part's base-locale file must be there: the SDK skips a missing one silently.
const pattern = settings["plugin.inlang.messageFormat"]?.pathPattern;
if (!pattern)
	fail("settings.json has no plugin.inlang.messageFormat.pathPattern");
const keys = new Set();
for (const p of [pattern].flat()) {
	const path = resolve(
		project,
		"..",
		p.replace("{locale}", settings.baseLocale),
	);
	if (!existsSync(path)) fail(`base-locale messages missing at ${path}`);
	for (const k of Object.keys(JSON.parse(readFileSync(path, "utf8"))))
		if (!k.startsWith("$")) keys.add(k);
}
if (keys.size === 0) fail("the base-locale message files are empty");

const messagesDir = join(outdir, "messages");
if (!existsSync(messagesDir))
	fail(`paraglide emitted no messages dir at ${messagesDir}`);
// paraglide writes one module per message plus an `_index.js` barrel.
const emitted = readdirSync(messagesDir).filter(
	(f) => f.endsWith(".js") && f !== "_index.js",
).length;

if (emitted < keys.size) {
	fail(
		`paraglide emitted ${emitted} messages but the base-locale files define ${keys.size}. ` +
			`The message-format plugin most likely failed to import — check the compile output for PluginImportError.`,
	);
}

console.log(
	`✔ i18n check: ${emitted} compiled messages for ${settings.locales.join(", ")}`,
);
