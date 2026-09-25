// Art & Metadata sources: the rules the host's merge depends on, and one source driven end to end
// against a recording host — the push, the Choose dialog's routes, and a pin re-resolving.
import { describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type { Punktfunk } from "@punktfunk/host";
import { Effect, Schema } from "effect";
import {
	defineMetadataPlugin,
	HIT_TTL_MS,
	identityOf,
	isFresh,
	type LibraryEntry,
	MISS_TTL_MS,
	ownView,
	toRow,
	wanted,
} from "../src/metadata/index.js";

const hades: LibraryEntry = {
	id: "steam:1145360",
	store: "steam",
	title: "Hades",
	art: { portrait: "/api/v1/library/art/steam:1145360/portrait?v=1" },
};
const offers = {
	art: ["portrait", "logo"] as const,
	meta: ["developer"] as const,
};

describe("metadata rules", () => {
	test("a launcher is never wanted; a gap is; complete own values are not", () => {
		expect(wanted({ ...hades, role: "launcher" }, offers, false)).toBe(false);
		expect(wanted(hades, offers, false)).toBe(true); // no logo, no developer
		const complete = {
			...hades,
			art: { portrait: "p", logo: "l" },
			developer: "Supergiant",
		};
		expect(wanted(complete, offers, false)).toBe(false);
		expect(wanted(complete, offers, true)).toBe(true); // replace looks up anyway
	});

	test("a borrowed value is still a gap; a pick is not", () => {
		const borrowed: LibraryEntry = {
			...hades,
			art: { portrait: "p", logo: "l" },
			developer: "Supergiant",
			filled: { logo: "other-source" },
		};
		expect(wanted(borrowed, offers, false)).toBe(true);
		const picked: LibraryEntry = {
			...borrowed,
			filled: { logo: "pick", portrait: "pick" },
		};
		expect(wanted(picked, offers, false)).toBe(false);
		expect(wanted(picked, offers, true)).toBe(false); // nothing left the operator did not pick
	});

	test("the own view drops borrowed values and keeps picks", () => {
		const e: LibraryEntry = {
			...hades,
			art: { portrait: "p", logo: "l", hero: "h" },
			developer: "D",
			release_year: 2020,
			filled: { logo: "sgdb", hero: "pick", developer: "libretro" },
		};
		const own = ownView(e);
		expect(own.art).toEqual({ portrait: "p", hero: "h" });
		expect(own.developer).toBeUndefined();
		expect(own.release_year).toBe(2020);
		expect(own.filled).toBeUndefined();
	});

	test("identity ignores borrowed fields and follows the pin", () => {
		const a = identityOf({ ...hades, platform: "PC" }, undefined);
		const borrowed = identityOf(
			{ ...hades, platform: "PC", developer: "D", filled: { developer: "x" } },
			undefined,
		);
		expect(borrowed).toBe(a);
		expect(identityOf({ ...hades, platform: "PC" }, "123")).not.toBe(a);
		const ordered = identityOf(
			{ ...hades, ids: { b: "2", a: "1" } },
			undefined,
		);
		expect(ordered).toBe(
			identityOf({ ...hades, ids: { a: "1", b: "2" } }, undefined),
		);
	});

	test("a hit keeps longer than a miss", () => {
		const base = { v: 1, identity: "i", at: 0, match: null };
		const hit = { ...base, found: { art: { logo: "https://x/l.png" } } };
		const miss = { ...base, found: null };
		expect(isFresh(hit, "i", 1, HIT_TTL_MS - 1)).toBe(true);
		expect(isFresh(miss, "i", 1, MISS_TTL_MS + 1)).toBe(false);
		expect(isFresh(hit, "other", 1, 0)).toBe(false);
		expect(isFresh(hit, "i", 2, 0)).toBe(false);
	});

	test("a row carries offered fields and http(s) art only", () => {
		const row = toRow(
			"steam:1",
			{
				art: {
					portrait: "https://x/p.png",
					logo: "data:image/png;base64,AA",
					hero: "https://x/h.png",
				},
				meta: { developer: "D", publisher: "P" },
			},
			offers,
		);
		expect(row).toEqual({
			id: "steam:1",
			art: { portrait: "https://x/p.png" },
			meta: { developer: "D" },
		});
		expect(
			toRow("steam:1", { art: { hero: "https://x/h.png" } }, offers),
		).toBeUndefined();
	});
});

const waitFor = async <T>(
	probe: () => T | undefined,
	ms = 5000,
): Promise<T> => {
	const until = Date.now() + ms;
	while (Date.now() < until) {
		const v = probe();
		if (v !== undefined) return v;
		await new Promise((r) => setTimeout(r, 20));
	}
	throw new Error("timed out waiting");
};

interface Call {
	readonly method: string;
	readonly path: string;
	readonly body: unknown;
}

describe("defineMetadataPlugin", () => {
	test("pushes, serves the Choose dialog, and re-resolves on a pin", async () => {
		const previous = process.env.PUNKTFUNK_CONFIG_DIR;
		process.env.PUNKTFUNK_CONFIG_DIR = fs.mkdtempSync(
			path.join(os.tmpdir(), "pf-metadata-"),
		);
		const library: LibraryEntry[] = [
			hades,
			{ id: "custom:abc", store: "custom", title: "Chrono Trigger" },
			{ id: "steam:ui", store: "steam", title: "Steam", role: "launcher" },
		];
		const calls: Call[] = [];
		const pf = {
			request: async (method: string, p: string, body?: unknown) => {
				calls.push({ method, path: p, body });
				if (method === "GET" && p === "/library") return library;
				if (method === "GET" && p === "/library/metadata") {
					return [{ id: "fake", replace: false }];
				}
				if (method === "GET" && p === "/plugins") {
					return [{ id: "fake", category: "metadata" }];
				}
				return {};
			},
			events: { on: () => () => {} },
		} as unknown as Punktfunk;
		const plugin = defineMetadataPlugin({
			name: "fake",
			configSchema: Schema.Struct({}),
			matching: "search",
			offers,
			match: (e, _cfg, pin) =>
				Effect.succeed({ key: pin ?? `m-${e.title}`, label: e.title }),
			fetch: (m) =>
				Effect.succeed({
					art: {
						portrait: `https://cdn/${m.key}/p.png`,
						logo: `https://cdn/${m.key}/l.png`,
					},
					meta: { developer: "Dev", publisher: "Pub" },
				}),
			search: (term) =>
				Effect.succeed([
					{ key: "m-other", label: `${term}!`, thumb: "file:///etc/passwd" },
				]),
		});
		const main = plugin.def.main as (pf: Punktfunk) => Promise<void>;
		const done = main(pf);
		const pushes = () =>
			calls.filter(
				(c) => c.method === "PUT" && c.path === "/library/metadata/fake",
			);
		try {
			const first = (await waitFor(() => pushes()[0])).body as {
				matching: string;
				entries: { id: string; art?: unknown; meta?: unknown }[];
			};
			expect(first.matching).toBe("search");
			expect(first.entries.map((e) => e.id)).toEqual([
				"custom:abc",
				"steam:1145360",
			]);
			expect(first.entries[1]).toEqual({
				id: "steam:1145360",
				art: {
					portrait: "https://cdn/m-Hades/p.png",
					logo: "https://cdn/m-Hades/l.png",
				},
				meta: { developer: "Dev" },
			});

			const reg = await waitFor(() =>
				calls.find((c) => c.method === "PUT" && c.path === "/plugins/fake"),
			);
			const ui = (
				reg.body as { ui: { port: number; secret: string }; category: string }
			).ui;
			expect((reg.body as { category: string }).category).toBe("metadata");
			const at = (p: string, init?: RequestInit) =>
				fetch(`http://127.0.0.1:${ui.port}/__metadata/${p}`, {
					...init,
					headers: { authorization: `Bearer ${ui.secret}` },
				});
			const q = `entry=${encodeURIComponent("steam:1145360")}`;

			const status = (await (await at("status")).json()) as Record<
				string,
				unknown
			>;
			expect(status).toMatchObject({
				ready: true,
				wanted: 2,
				found: 2,
				searchable: true,
			});

			const images = (await (await at(`images?${q}&kind=logo`)).json()) as {
				images: { url: string }[];
			};
			expect(images.images).toEqual([{ url: "https://cdn/m-Hades/l.png" }]);
			expect((await at(`images?${q}&kind=banner`)).status).toBe(400);
			expect((await at("images?entry=nope&kind=logo")).status).toBe(400);

			const search = (await (
				await at("search", {
					method: "POST",
					body: JSON.stringify({ entry: "steam:1145360", term: "Hades" }),
				})
			).json()) as {
				candidates: { key: string; label: string; thumb: string | null }[];
			};
			expect(search.candidates).toEqual([
				{ key: "m-other", label: "Hades!", thumb: null },
			]);

			const pinned = (await (
				await at("match", {
					method: "PUT",
					body: JSON.stringify({ entry: "steam:1145360", key: "m-other" }),
				})
			).json()) as { match: { key: string; label: string }; pinned: boolean };
			expect(pinned).toEqual({
				match: { key: "m-other", label: "Hades" },
				pinned: true,
			});
			const second = (await waitFor(() => pushes()[1])).body as {
				entries: { id: string; art?: { logo?: string } }[];
			};
			expect(
				second.entries.find((e) => e.id === "steam:1145360")?.art?.logo,
			).toBe("https://cdn/m-other/l.png");
		} finally {
			process.emit("SIGTERM");
			await done;
			if (previous === undefined) delete process.env.PUNKTFUNK_CONFIG_DIR;
			else process.env.PUNKTFUNK_CONFIG_DIR = previous;
		}
	});
});
