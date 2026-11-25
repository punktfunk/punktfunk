import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { artVersion, isHttpUrl, versionOf } from "./metadata";

const vectors = JSON.parse(
	readFileSync(
		join(import.meta.dir, "../../../clients/shared/library-id-vectors.json"),
		"utf8",
	),
) as {
	urls: { value: string; fill?: string; count?: number; valid: boolean }[];
	art_versions: { url: string; v: string }[];
};

for (const c of vectors.urls) {
	const url = c.value + (c.fill ?? "").repeat(c.count ?? 0);
	test(`${c.valid ? "accepts" : "refuses"} ${JSON.stringify(url.slice(0, 40))} (${url.length})`, () => {
		expect(isHttpUrl(url)).toBe(c.valid);
	});
}

for (const c of vectors.art_versions) {
	test(`versions ${c.url} as the host's art proxy does`, async () => {
		expect(await artVersion(c.url)).toBe(c.v);
		expect(versionOf(`/api/v1/library/art/steam:570/portrait?v=${c.v}`)).toBe(
			c.v,
		);
	});
}

test("an art value that is no proxy path has no version", () => {
	expect(versionOf("https://cdn.example/p.png?v=1")).toBeNull();
	expect(versionOf(null)).toBeNull();
	expect(
		versionOf("/api/v1/library/art/steam:570/hero?v=0123456789abcdef&as=anna"),
	).toBe("0123456789abcdef");
});
