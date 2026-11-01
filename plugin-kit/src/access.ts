import * as os from "node:os";
import * as path from "node:path";
import { Effect } from "effect";
import { HostClient } from "./host-client.js";
import { dirAccess } from "./library/parsers/fs.js";

export interface AccessRequestPath {
	readonly path: string;
	readonly write?: boolean;
	/**
	 * The folder and the files directly in it, nothing below. A Windows library folder holding a
	 * game tree needs no more, and this lands at once where a whole folder rewrites every file in
	 * the tree. One `*` component in the path names each folder under it (`D:\Games\*`): one yes
	 * covers them all. A host older than 0.46 ignores the flag and asks for the whole folder, and
	 * refuses a pattern, so ask for the folders themselves beside it.
	 */
	readonly files?: boolean;
}

const pathOf = (entry: string | AccessRequestPath): string =>
	typeof entry === "string" ? entry : entry.path;

export interface AccessRequestOutcome {
	readonly path: string;
	readonly outcome: string;
}

/** Each kind of failed request is logged once per process, not on every poll. */
const warned = new Set<string>();

const statusOf = (value: unknown): number | undefined => {
	if (typeof value !== "object" || value === null) return undefined;
	const record = value as Record<string, unknown>;
	if (typeof record.status === "number") return record.status;
	if (typeof record.statusCode === "number") return record.statusCode;
	return statusOf(record.cause);
};

const why = (status: number | undefined, message: string): string => {
	if (status === 404)
		return "the host is too old for folder access requests — update it to let plugins ask for launcher folders";
	if (status === 403)
		return "this plugin has no identity of its own on this host, so it can't ask for folders — allow one with `punktfunk-host plugins grant`";
	return `folder access request failed — ${message}`;
};

/** `~/x` → `<home>/x`, as manifest paths expand; the host refuses anything relative. */
const expand = (p: string): string =>
	p.startsWith("~/") ? path.join(os.homedir(), p.slice(2)) : p;

/**
 * Ask the host to install emulators for the operator to approve; this never installs anything
 * itself. Each outcome names the folder the install lands in — the grant that follows — or
 * `refused:not_offered` / `refused:not_in_catalog`. Same posture as {@link requestAccess}: a
 * host that can't take the request is logged once.
 */
export const requestEmulators = (
	emulators: ReadonlyArray<string>,
	reason?: string,
): Effect.Effect<AccessRequestOutcome[], never, HostClient> =>
	post({ paths: [], emulators: [...emulators], ...(reason ? { reason } : {}) });

/**
 * Ask the host for libretro cores in its RetroArch; the operator approves, hermir fetches them
 * from the buildbot. Outcomes name RetroArch's cores folder, or `refused:no_retroarch`.
 */
export const requestCores = (
	cores: ReadonlyArray<string>,
	reason?: string,
): Effect.Effect<AccessRequestOutcome[], never, HostClient> =>
	post({ paths: [], cores: [...cores], ...(reason ? { reason } : {}) });

/**
 * Ask the host to put these folders before the operator; this never grants access itself. It
 * never fails either: a host that can't take the request is logged once, and the scan goes on
 * with what it can reach.
 */
export const requestAccess = (
	paths: ReadonlyArray<string | AccessRequestPath>,
	reason?: string,
): Effect.Effect<AccessRequestOutcome[], never, HostClient> =>
	Effect.gen(function* () {
		const body = {
			paths: paths.map((entry) =>
				typeof entry === "string"
					? { path: expand(entry) }
					: { ...entry, path: expand(entry.path) },
			),
			...(reason ? { reason } : {}),
		};
		return yield* post(body);
	});

/** One request to the host, its outcomes filtered to the rows that are what they claim. */
const post = (
	body: object,
): Effect.Effect<AccessRequestOutcome[], never, HostClient> =>
	Effect.gen(function* () {
		const host = yield* HostClient;
		return yield* host.request("POST", "/plugin-access/requests", body).pipe(
			Effect.map((value) =>
				(Array.isArray(value) ? value : []).filter(
					(row): row is AccessRequestOutcome =>
						typeof row === "object" &&
						row !== null &&
						typeof (row as { path?: unknown }).path === "string" &&
						typeof (row as { outcome?: unknown }).outcome === "string",
				),
			),
			Effect.catch((error) => {
				const text = why(statusOf(error.cause), error.message);
				if (warned.has(text)) return Effect.succeed([]);
				warned.add(text);
				return Effect.logWarning(text).pipe(Effect.as([]));
			}),
		);
	});

/** Folders unusable now. Inside a sandbox, missing may mean merely unbound, so ask the host. */
export const unreachable = <T extends string | AccessRequestPath>(
	dirs: ReadonlyArray<T>,
): T[] => {
	const sandboxed = !!process.env.PUNKTFUNK_MGMT_UNIX;
	const seen = new Map<string, T>();
	for (const entry of dirs) {
		const dir = expand(pathOf(entry));
		if (seen.has(dir)) continue;
		seen.set(
			dir,
			(typeof entry === "string"
				? dir
				: { ...(entry as AccessRequestPath), path: dir }) as T,
		);
	}
	return [...seen]
		.filter(([dir]) => {
			const access = dirAccess(dir);
			return access === "denied" || (sandboxed && access === "missing");
		})
		.map(([, entry]) => entry);
};

/** `entries` in a stable order, whatever mix of spellings they arrive in. */
export const byPath = <T extends string | AccessRequestPath>(
	entries: ReadonlyArray<T>,
): T[] => [...entries].sort((a, b) => pathOf(a).localeCompare(pathOf(b)));
