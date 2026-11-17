// Schema-driven plugin config with raw round-trip semantics.
//
// The invariant: the file on disk is ALWAYS the operator-authored (raw / Encoded) shape;
// defaults live ONLY in the Schema (`Schema.withDecodingDefaultKey(..., { encodingStrategy:
// "omit" })`) and are applied at decode time. `saveRaw` validates by decoding but persists
// the raw shape verbatim — a UI save never bakes defaults into the file.
//
// Also: state dir 0700, atomic temp+rename 0600 writes (paths.ts), POSIX group/world-writable
// refusal (config controls commands run as the host user), missing file == empty config.
import * as fs from "node:fs";
import { Effect, PubSub, Schema, Stream } from "effect";
import {
	ConfigParseError,
	ConfigPermissionError,
	type ConfigWriteError,
} from "./errors.js";
import { PluginInfo } from "./host-client.js";
import { atomicWriteFile, ensureStateDir, statePath } from "./paths.js";

export interface ConfigService<S extends Schema.Top> {
	/** Decode the raw file with Schema defaults applied. Missing file → all defaults. */
	readonly load: Effect.Effect<
		S["Type"],
		ConfigParseError | ConfigPermissionError
	>;
	/** The operator-authored shape, validated but NOT defaulted — what the UI edits. */
	readonly loadRaw: Effect.Effect<
		S["Encoded"],
		ConfigParseError | ConfigPermissionError
	>;
	/**
	 * Validate-by-decode, persist the RAW shape verbatim (atomic), emit the decoded
	 * config on `changes`, and return it.
	 */
	readonly saveRaw: (
		raw: unknown,
	) => Effect.Effect<S["Type"], ConfigParseError | ConfigWriteError>;
	/** Emits the decoded config after every successful `saveRaw`. */
	readonly changes: Stream.Stream<S["Type"]>;
	/** Absolute path of the config file (status views). */
	readonly path: string;
}

/** Refuse a group/world-writable config file (POSIX only; Windows state dir is DACL'd). */
const checkNotWorldWritable = (
	file: string,
): Effect.Effect<void, ConfigPermissionError> =>
	Effect.suspend(() => {
		if (process.platform === "win32") return Effect.void;
		let mode: number;
		try {
			mode = fs.statSync(file).mode;
		} catch {
			return Effect.void; // absent — nothing to guard
		}
		return (mode & 0o022) !== 0
			? Effect.fail(new ConfigPermissionError({ path: file, mode }))
			: Effect.void;
	});

const readRawObject = (
	file: string,
): Effect.Effect<unknown, ConfigParseError | ConfigPermissionError> =>
	checkNotWorldWritable(file).pipe(
		Effect.flatMap(() =>
			Effect.suspend(() => {
				let text: string;
				try {
					text = fs.readFileSync(file, "utf8");
				} catch {
					return Effect.succeed({} as unknown); // missing file == empty config
				}
				try {
					return Effect.succeed(JSON.parse(text) as unknown);
				} catch (e) {
					return Effect.fail(
						new ConfigParseError({ path: file, issue: String(e) }),
					);
				}
			}),
		),
	);

export const makeConfigService = <S extends Schema.Top>(opts: {
	readonly schema: S;
	readonly fileName?: string;
}): Effect.Effect<ConfigService<S>, never, PluginInfo> =>
	Effect.gen(function* () {
		const info = yield* PluginInfo;
		const file = statePath(info.name, opts.fileName ?? "config.json");
		const hub = yield* PubSub.unbounded<S["Type"]>();

		const decode = (raw: unknown): Effect.Effect<S["Type"], ConfigParseError> =>
			Schema.decodeUnknownEffect(opts.schema)(raw).pipe(
				Effect.mapError(
					(e) => new ConfigParseError({ path: file, issue: String(e) }),
				),
			) as Effect.Effect<S["Type"], ConfigParseError>;

		const load = readRawObject(file).pipe(Effect.flatMap(decode));

		// Validated (a broken file must not masquerade as authored config), returned verbatim.
		const loadRaw = readRawObject(file).pipe(
			Effect.tap(decode),
			Effect.map((raw) => raw as S["Encoded"]),
		);

		const saveRaw = (raw: unknown) =>
			decode(raw).pipe(
				Effect.tap(() => ensureStateDir(info.name)),
				Effect.tap(() =>
					atomicWriteFile(file, `${JSON.stringify(raw, null, 2)}\n`),
				),
				Effect.tap((decoded) => PubSub.publish(hub, decoded)),
			);

		return {
			load,
			loadRaw,
			saveRaw,
			changes: Stream.fromPubSub(hub),
			path: file,
		} satisfies ConfigService<S>;
	});

// A `Context.Service` class factory is deliberately NOT provided — plugins define their
// own service key over `ConfigService<their schema>` so the config type stays precise:
//
//   class RomConfig extends Context.Service<RomConfig, ConfigService<typeof RomConfigSchema>>()(
//     "rom-manager/Config",
//   ) {
//     static layer = Layer.effect(RomConfig)(makeConfigService({ schema: RomConfigSchema }))
//   }
