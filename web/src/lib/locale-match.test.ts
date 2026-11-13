import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { localeFallback, matchLocale } from "./locale-match";

const shipped = ["en", "de", "es", "fr", "ja", "ko", "zh-Hans", "zh-Hant"];

describe("matchLocale", () => {
	test("Chinese without a script takes the one its region writes", () => {
		expect(matchLocale(["zh-TW"], shipped)).toBe("zh-Hant");
		expect(matchLocale(["zh-HK"], shipped)).toBe("zh-Hant");
		expect(matchLocale(["zh-MO"], shipped)).toBe("zh-Hant");
		expect(matchLocale(["zh-CN"], shipped)).toBe("zh-Hans");
		expect(matchLocale(["zh-SG"], shipped)).toBe("zh-Hans");
		expect(matchLocale(["zh"], shipped)).toBe("zh-Hans");
	});

	test("Chinese that names its script keeps it", () => {
		expect(matchLocale(["zh-Hant-HK"], shipped)).toBe("zh-Hant");
		expect(matchLocale(["zh-Hans-CN"], shipped)).toBe("zh-Hans");
		expect(matchLocale(["zh-Hant-CN"], shipped)).toBe("zh-Hant");
		expect(matchLocale(["zh-Hans-TW"], shipped)).toBe("zh-Hans");
	});

	test("a regional tag drops to its language", () => {
		expect(matchLocale(["ja-JP"], shipped)).toBe("ja");
		expect(matchLocale(["ko-KR"], shipped)).toBe("ko");
		expect(matchLocale(["de-AT"], shipped)).toBe("de");
		expect(matchLocale(["en-US"], shipped)).toBe("en");
	});

	test("a language not shipped matches nothing", () => {
		expect(matchLocale(["sv-SE"], shipped)).toBeUndefined();
		expect(matchLocale(["ja-JP"], ["en", "de"])).toBeUndefined();
		expect(matchLocale(["zh-TW"], ["en", "zh-Hans"])).toBeUndefined();
		expect(matchLocale([], shipped)).toBeUndefined();
	});

	test("the list is read in order and the first match wins", () => {
		expect(matchLocale(["sv-SE", "de-AT", "en"], shipped)).toBe("de");
		expect(matchLocale(["zh-TW", "en"], shipped)).toBe("zh-Hant");
		expect(matchLocale(["fr-CA", "de"], ["en", "de"])).toBe("de");
	});

	test("case and underscores do not matter, and the shipped spelling comes back", () => {
		expect(matchLocale(["ZH_tw"], shipped)).toBe("zh-Hant");
		expect(matchLocale([" DE-at "], shipped)).toBe("de");
	});

	test("every other Portuguese reads the one that ships", () => {
		const web = resolve(import.meta.dir, "../..");
		const locales = JSON.parse(
			readFileSync(join(web, "../l10n/locales.json"), "utf8"),
		) as { shipped: string[]; fallback: Record<string, string> };
		expect(localeFallback).toEqual(locales.fallback);
		for (const tag of ["pt", "pt-PT", "pt_PT.UTF-8", "pt-BR"]) {
			expect(matchLocale([tag], locales.shipped, localeFallback)).toBe("pt-BR");
		}
	});

	test("replays the shared pick cases", () => {
		const web = resolve(import.meta.dir, "../..");
		const { pick } = JSON.parse(
			readFileSync(join(web, "../l10n/vectors.json"), "utf8"),
		) as {
			pick: {
				os: string[];
				shipped: string[];
				fallback: Record<string, string>;
				want: string;
			}[];
		};
		for (const c of pick) {
			// The source language is shipped first and is what no match falls back to.
			const got = matchLocale(c.os, c.shipped, c.fallback) ?? c.shipped[0];
			expect(got, JSON.stringify(c.os)).toBe(c.want);
		}
	});
});
