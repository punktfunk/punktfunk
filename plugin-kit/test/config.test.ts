// ConfigService semantics: raw round-trip, schema defaults at decode only, unknown keys kept,
// missing file == defaults, world-writable refusal, changes stream.
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { Effect, Queue, Schema, Stream } from "effect";
import {
	type ConfigService,
	makeConfigService,
	pluginInfoLayer,
	pluginStateDir,
} from "../src/index.js";

const TestSchema = Schema.Struct({
	roots: Schema.Array(Schema.String).pipe(
		Schema.withDecodingDefaultKey(Effect.succeed([]), {
			encodingStrategy: "omit",
		}),
	),
	sync: Schema.Struct({
		pollMinutes: Schema.Number.pipe(
			Schema.withDecodingDefaultKey(Effect.succeed(15), {
				encodingStrategy: "omit",
			}),
		),
		watch: Schema.Boolean.pipe(
			Schema.withDecodingDefaultKey(Effect.succeed(true), {
				encodingStrategy: "omit",
			}),
		),
	}).pipe(
		Schema.withDecodingDefaultKey(Effect.succeed({}), {
			encodingStrategy: "omit",
		}),
	),
});

const PLUGIN = "kit-config-test";
let tmp: string;

const withService = <A>(
	f: (svc: ConfigService<typeof TestSchema>) => Effect.Effect<A, unknown>,
): Promise<A> =>
	Effect.runPromise(
		makeConfigService({ schema: TestSchema }).pipe(
			Effect.flatMap(f),
			Effect.provide(pluginInfoLayer({ name: PLUGIN })),
		) as Effect.Effect<A, never>,
	);

beforeEach(() => {
	tmp = fs.mkdtempSync(path.join(os.tmpdir(), "pf-kit-config-"));
	process.env.PUNKTFUNK_CONFIG_DIR = tmp;
});
afterEach(() => {
	delete process.env.PUNKTFUNK_CONFIG_DIR;
	fs.rmSync(tmp, { recursive: true, force: true });
});

describe("ConfigService", () => {
	test("missing file decodes to full defaults", async () => {
		const loaded = await withService((svc) => svc.load);
		expect(loaded).toEqual({
			roots: [],
			sync: { pollMinutes: 15, watch: true },
		});
	});

	test("saveRaw persists the RAW shape verbatim (no defaults baked in)", async () => {
		await withService((svc) => svc.saveRaw({ roots: ["/roms"] }));
		const onDisk = JSON.parse(
			fs.readFileSync(path.join(pluginStateDir(PLUGIN), "config.json"), "utf8"),
		);
		expect(onDisk).toEqual({ roots: ["/roms"] }); // no sync block materialized
		const loaded = await withService((svc) => svc.load);
		expect(loaded.sync.pollMinutes).toBe(15);
	});

	test("saveRaw rejects an invalid raw config and leaves the file untouched", async () => {
		await withService((svc) => svc.saveRaw({ roots: ["/a"] }));
		const before = fs.readFileSync(
			path.join(pluginStateDir(PLUGIN), "config.json"),
			"utf8",
		);
		const exit = await withService((svc) =>
			Effect.exit(svc.saveRaw({ roots: "not-an-array" })),
		);
		expect(exit._tag).toBe("Failure");
		const after = fs.readFileSync(
			path.join(pluginStateDir(PLUGIN), "config.json"),
			"utf8",
		);
		expect(after).toBe(before);
	});

	test("unknown keys in the file are tolerated and survive loadRaw", async () => {
		fs.mkdirSync(pluginStateDir(PLUGIN), { recursive: true, mode: 0o700 });
		fs.writeFileSync(
			path.join(pluginStateDir(PLUGIN), "config.json"),
			JSON.stringify({ roots: [], ui: { port: 5885 }, devEntry: true }),
			{ mode: 0o600 },
		);
		const raw = (await withService((svc) => svc.loadRaw)) as Record<
			string,
			unknown
		>;
		expect(raw.ui).toEqual({ port: 5885 }); // verbatim — a save decides what survives
	});

	test("refuses a group/world-writable config file (POSIX)", async () => {
		if (process.platform === "win32") return;
		fs.mkdirSync(pluginStateDir(PLUGIN), { recursive: true, mode: 0o700 });
		const file = path.join(pluginStateDir(PLUGIN), "config.json");
		fs.writeFileSync(file, "{}");
		fs.chmodSync(file, 0o666); // bypass umask — writeFileSync's mode is masked
		const exit = await withService((svc) => Effect.exit(svc.load));
		expect(exit._tag).toBe("Failure");
		if (exit._tag === "Failure") {
			expect(String(exit.cause)).toContain("ConfigPermissionError");
		}
	});

	test("changes stream emits the decoded config after saveRaw", async () => {
		const decoded = await withService((svc) =>
			Effect.scoped(
				Effect.gen(function* () {
					const queue = yield* svc.changes.pipe(
						Stream.toQueue({ capacity: "unbounded" }),
					);
					yield* Effect.yieldNow;
					yield* svc.saveRaw({ roots: ["/x"], sync: { pollMinutes: 5 } });
					return [yield* Queue.take(queue)];
				}),
			),
		);
		expect(decoded).toEqual([
			{ roots: ["/x"], sync: { pollMinutes: 5, watch: true } },
		]);
	});
});
