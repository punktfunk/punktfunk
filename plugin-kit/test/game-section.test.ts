// The `/__game?entry=<id>` wire the console's entry page codes against, driven directly.
import { describe, expect, test } from "bun:test";
import { Effect, Schema } from "effect";
import { handedPath, makeGameHandler } from "../src/ui-server.js";

const Section = Schema.Struct({
	enabled: Schema.Boolean,
	paths: Schema.Array(handedPath({ write: true })),
});

const setup = () => {
	const saved = new Map<string, unknown>();
	const handler = makeGameHandler({
		schema: Section,
		load: (id) =>
			Effect.succeed(
				id.startsWith("steam:")
					? (saved.get(id) ?? { enabled: false, paths: [] })
					: undefined,
			),
		save: (id, value) => Effect.sync(() => void saved.set(id, value)),
		status: (id) =>
			Effect.succeed([
				{
					level: "warn" as const,
					text: `slot for ${id}\u0007 ${"x".repeat(200)}`,
				},
				...Array.from({ length: 10 }, () => ({
					level: "info" as const,
					text: "l",
				})),
			]),
	});
	return { saved, handler };
};

const url = (entry: string) =>
	`http://x/__game?entry=${encodeURIComponent(entry)}`;

describe("game section", () => {
	test("GET answers schema, value and capped status lines", async () => {
		const { handler } = setup();
		const res = await handler(new Request(url("steam:504230")));
		expect(res.status).toBe(200);
		const body = (await res.json()) as {
			schema: unknown;
			value: unknown;
			status: { level: string; text: string }[];
		};
		// The console finds the paths to grant by this format; the derivation must keep it.
		expect(JSON.stringify(body.schema)).toContain('"format":"pf:path:write"');
		expect(body.value).toEqual({ enabled: false, paths: [] });
		expect(body.status).toHaveLength(8);
		expect(body.status[0].level).toBe("warn");
		expect(body.status[0].text.length).toBe(120);
		expect(body.status[0].text).not.toContain("\u0007");
	});

	test("the entry's page route goes out only as one segment", async () => {
		const page = (route: string) =>
			makeGameHandler({
				schema: Section,
				load: () => Effect.succeed({ enabled: true, paths: [] }),
				save: () => Effect.void,
				page: () => Effect.succeed(route),
			});
		const read = async (route: string) =>
			(
				(await (await page(route)(new Request(url("steam:1")))).json()) as {
					page?: string;
				}
			).page;
		expect(await read("game.cm9tbS8x")).toBe("game.cm9tbS8x");
		expect(await read("game/../../settings")).toBeUndefined();
		expect(await read("https://example.com")).toBeUndefined();
		expect((await setup().handler(new Request(url("steam:1")))).ok).toBe(true);
	});

	test("no section for an entry is a 404", async () => {
		const { handler } = setup();
		const res = await handler(new Request(url("custom:abc")));
		expect(res.status).toBe(404);
	});

	test("an id with a slash survives the query string", async () => {
		const { handler } = setup();
		const res = await handler(new Request(url("steam:a/b c")));
		expect(res.status).toBe(200);
	});

	test("a malformed id is refused", async () => {
		const { handler } = setup();
		expect((await handler(new Request(url("no-colon")))).status).toBe(400);
		expect((await handler(new Request("http://x/__game"))).status).toBe(400);
	});

	test("PUT decodes before it saves", async () => {
		const { handler, saved } = setup();
		const bad = await handler(
			new Request(url("steam:1"), {
				method: "PUT",
				body: JSON.stringify({ enabled: "yes" }),
			}),
		);
		expect(bad.status).toBe(400);
		expect(saved.size).toBe(0);
		const good = await handler(
			new Request(url("steam:1"), {
				method: "PUT",
				body: JSON.stringify({ enabled: true, paths: ["/home/a/.config/x"] }),
			}),
		);
		expect(good.status).toBe(200);
		expect(saved.get("steam:1")).toEqual({
			enabled: true,
			paths: ["/home/a/.config/x"],
		});
	});
});
