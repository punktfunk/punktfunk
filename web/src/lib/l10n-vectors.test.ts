// l10n/vectors.json through Paraglide, the console's reader of the shared message files. pf-l10n's
// Rust tests replay the same file, so a case that passes there and fails here is the two readers
// disagreeing. The `pick` cases are the OS language picker, which the console does not use.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import {
	mkdirSync,
	mkdtempSync,
	readFileSync,
	rmSync,
	writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { compile } from "@inlang/paraglide-js";
import { m } from "@/paraglide/messages";

const web = resolve(import.meta.dir, "../..");
const vectors = JSON.parse(
	readFileSync(join(web, "../l10n/vectors.json"), "utf8"),
) as {
	messages: Record<string, Record<string, unknown>>;
	format: {
		key: string;
		lang: string;
		args: Record<string, unknown>;
		want: string;
	}[];
};
const settings = JSON.parse(
	readFileSync(join(web, "project.inlang/settings.json"), "utf8"),
) as { modules: string[] };

type Message = (
	args: Record<string, unknown>,
	options: { locale: string },
) => string;
const dir = mkdtempSync(join(tmpdir(), "pf-l10n-vectors-"));
let compiled: Record<string, Message>;

beforeAll(async () => {
	const locales = Object.keys(vectors.messages);
	mkdirSync(join(dir, "project.inlang"));
	writeFileSync(
		join(dir, "project.inlang/settings.json"),
		JSON.stringify({
			baseLocale: locales[0],
			locales,
			modules: settings.modules.map((p) => resolve(web, p)),
			"plugin.inlang.messageFormat": {
				pathPattern: "./messages/{locale}.json",
			},
		}),
	);
	mkdirSync(join(dir, "messages"));
	for (const [locale, messages] of Object.entries(vectors.messages))
		writeFileSync(
			join(dir, `messages/${locale}.json`),
			JSON.stringify(messages),
		);
	await compile({
		project: join(dir, "project.inlang"),
		outdir: join(dir, "out"),
	});
	compiled = (await import(join(dir, "out/messages.js"))).m;
});

afterAll(() => rmSync(dir, { recursive: true, force: true }));

describe("vectors.json through Paraglide", () => {
	for (const c of vectors.format) {
		test(`${c.key} in ${c.lang} with ${JSON.stringify(c.args)}`, () => {
			const message = compiled[c.key];
			expect(message).toBeDefined();
			expect(message?.(c.args, { locale: c.lang })).toBe(c.want);
		});
	}
});

describe("the console's plurals", () => {
	test("English follows the count", () => {
		expect(m.store_updates_pending({ count: 1 }, { locale: "en" })).toBe(
			"1 plugin can be updated",
		);
		expect(m.store_updates_pending({ count: 3 }, { locale: "en" })).toBe(
			"3 plugins can be updated",
		);
		expect(m.library_entry_launches({ count: 1 }, { locale: "en" })).toBe(
			"Launched once",
		);
		expect(
			m.library_count_shown({ shown: 0, count: 1 }, { locale: "en" }),
		).toBe("0 of 1 game");
	});

	test("German follows the count", () => {
		expect(m.store_updates_pending({ count: 1 }, { locale: "de" })).toBe(
			"Für 1 Plugin gibt es ein Update",
		);
		expect(m.store_updates_pending({ count: 3 }, { locale: "de" })).toBe(
			"Für 3 Plugins gibt es Updates",
		);
	});

	test("a translation can be a plural of its own", () => {
		expect(m.library_entry_launches({ count: 1 }, { locale: "de" })).toBe(
			"Einmal gestartet",
		);
		expect(m.library_entry_launches({ count: 4 }, { locale: "de" })).toBe(
			"4-mal gestartet",
		);
	});

	test("a category the message lacks reads the n=* variant", () => {
		expect(
			m.library_entry_launches({ count: 1_000_000 }, { locale: "es" }),
		).toBe("Iniciado 1000000 veces");
	});

	test("shared parts compile in", () => {
		expect(m.common_cancel({}, { locale: "de" })).toBe("Abbrechen");
		expect(m.host_power_reboot({}, { locale: "es" })).toBe("Reiniciar host");
	});
});
