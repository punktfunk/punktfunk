/**
 * Titles a plugin installs on the host. The host asks at `POST /__install` (install, pause,
 * cancel, remove); the plugin moves the bytes and reports progress to
 * `PUT /library/provider/{id}/downloads`, which the host turns into speed, time left, the
 * console's Downloads list and every client's launch screen.
 */
import { Data, Effect, type Scope } from "effect";
import { HostClient } from "./host-client.js";

export type InstallAction = "start" | "pause" | "cancel" | "uninstall";

/** Which title the host means: its library id and this plugin's own key for it. */
export interface InstallAsk {
	readonly app: string;
	readonly externalId: string;
}

/** The plugin can't do what was asked; `message` is one sentence the operator or player reads. */
export class InstallRefused extends Data.TaggedError("InstallRefused")<{
	readonly message: string;
}> {}

/** The title isn't one of this plugin's. */
export class NotMyTitle extends Data.TaggedError("NotMyTitle")<{
	readonly externalId: string;
}> {}

/**
 * What a plugin does when the host asks. `start` installs, or resumes a paused download, and
 * answers once the work is under way, not when it ends. `pause` keeps the partial files;
 * `cancel` discards them; `uninstall` removes what the plugin downloaded for the title.
 */
export type ServeUiInstall = {
	readonly [A in InstallAction]: (
		ask: InstallAsk,
	) => Effect.Effect<void, InstallRefused | NotMyTitle | unknown>;
};

const ACTIONS: ReadonlyArray<InstallAction> = [
	"start",
	"pause",
	"cancel",
	"uninstall",
];

/**
 * The `/__install` handler. `start` and `pause` answer 202, `cancel` and `uninstall` 200;
 * {@link InstallRefused} answers 409 with its message, {@link NotMyTitle} 404, anything else
 * 500.
 */
export const makeInstallHandler =
	(install: ServeUiInstall) =>
	async (req: Request): Promise<Response> => {
		if (req.method !== "POST") {
			return new Response("method not allowed", { status: 405 });
		}
		let body: Record<string, unknown>;
		try {
			body = (await req.json()) as Record<string, unknown>;
		} catch (cause) {
			return Response.json(
				{ error: "body must be JSON", issue: String(cause) },
				{ status: 400 },
			);
		}
		const action = ACTIONS.find((a) => a === body?.action);
		if (
			!action ||
			typeof body.app !== "string" ||
			typeof body.external_id !== "string"
		) {
			return Response.json(
				{ error: "expected { app, external_id, action }" },
				{ status: 400 },
			);
		}
		const ask = { app: body.app, externalId: body.external_id };
		const done = action === "start" || action === "pause" ? 202 : 200;
		try {
			const answer = await Effect.runPromise(
				install[action](ask).pipe(
					Effect.as({ status: done, message: undefined as string | undefined }),
					Effect.catch((e) =>
						Effect.succeed(
							e instanceof InstallRefused
								? { status: 409, message: e.message }
								: e instanceof NotMyTitle
									? { status: 404, message: undefined }
									: { status: 500, message: String(e) },
						),
					),
				),
				{ signal: req.signal },
			);
			if (answer.status === 409) {
				return Response.json({ message: answer.message }, { status: 409 });
			}
			if (answer.status === 500) {
				return Response.json(
					{ error: "install failed", issue: answer.message },
					{ status: 500 },
				);
			}
			return new Response(null, { status: answer.status });
		} catch (cause) {
			return Response.json(
				{ error: "install failed", issue: String(cause) },
				{ status: 500 },
			);
		}
	};

export type DownloadState =
	| "queued"
	| "downloading"
	| "paused"
	| "installing"
	| "done"
	| "failed"
	| "cancelled";

/** One title's download as the plugin knows it. */
export interface DownloadRow {
	readonly externalId: string;
	readonly state: DownloadState;
	readonly doneBytes: number;
	/** Absent while the size isn't known. */
	readonly totalBytes?: number;
	/** What it's doing, in a few words: `File 2 of 3`, `Verifying`. */
	readonly phase?: string;
	/** Why it failed, one sentence for the player. */
	readonly error?: string;
}

const live = (s: DownloadState) =>
	s === "queued" || s === "downloading" || s === "installing";
const terminal = (s: DownloadState) =>
	s === "done" || s === "failed" || s === "cancelled";

export type SendResult = "ok" | "unsupported" | "failed";

export interface DownloadReporter {
	/** Record a title's row. It reaches the host within a second; rows not set again are restated. */
	readonly set: (row: DownloadRow) => void;
	/** `false` once the host answered 404 (it predates downloads); `undefined` before any answer. */
	readonly supported: () => boolean | undefined;
	/** Send what is pending now and wait for the answer. */
	readonly flush: () => Promise<void>;
	/** Stop the timers. Rows still pending are not sent. */
	readonly close: () => void;
}

/**
 * The host's rules for a report, kept here so a plugin can't break them: changes at most once a
 * second, and every 5 s while a row is live — a row the host hears nothing about for 30 s counts
 * as stalled. A finished, failed or cancelled row is sent once, then dropped.
 */
export const makeDownloadReporter = (
	send: (rows: ReadonlyArray<DownloadRow>) => Promise<SendResult>,
	timing: { readonly throttleMs?: number; readonly heartbeatMs?: number } = {},
): DownloadReporter => {
	const throttleMs = timing.throttleMs ?? 1000;
	const heartbeatMs = timing.heartbeatMs ?? 5000;
	const rows = new Map<string, DownloadRow>();
	let supported: boolean | undefined;
	let lastSent = 0;
	let timer: ReturnType<typeof setTimeout> | undefined;
	let inFlight: Promise<void> | undefined;
	let again = false;
	let closed = false;

	const flush = async (): Promise<void> => {
		if (inFlight) {
			again = true;
			return inFlight;
		}
		if (closed || supported === false || rows.size === 0) return;
		const batch = [...rows.values()];
		lastSent = Date.now();
		inFlight = send(batch).then((result) => {
			if (result === "unsupported") {
				supported = false;
				rows.clear();
				return;
			}
			if (result !== "ok") return;
			supported = true;
			for (const sent of batch) {
				// Sent once: unless the title moved on since, its terminal row is done.
				if (terminal(sent.state) && rows.get(sent.externalId) === sent) {
					rows.delete(sent.externalId);
				}
			}
		});
		try {
			await inFlight;
		} finally {
			inFlight = undefined;
			if (again) {
				again = false;
				schedule();
			}
		}
	};

	const schedule = () => {
		if (timer || closed) return;
		const wait = Math.max(0, lastSent + throttleMs - Date.now());
		timer = setTimeout(() => {
			timer = undefined;
			void flush();
		}, wait);
	};

	const heartbeat = setInterval(() => {
		if ([...rows.values()].some((r) => live(r.state))) void flush();
	}, heartbeatMs);

	return {
		set: (row) => {
			if (closed || supported === false) return;
			rows.set(row.externalId, row);
			schedule();
		},
		supported: () => supported,
		flush,
		close: () => {
			closed = true;
			clearInterval(heartbeat);
			if (timer) clearTimeout(timer);
		},
	};
};

const statusOf = (value: unknown): number | undefined => {
	if (typeof value !== "object" || value === null) return undefined;
	const record = value as Record<string, unknown>;
	if (typeof record.status === "number") return record.status;
	if (typeof record.statusCode === "number") return record.statusCode;
	return statusOf(record.cause);
};

/**
 * A {@link DownloadReporter} for `provider`'s titles, sending through the host client; closed
 * with the scope.
 */
export const downloadReporter = (
	provider: string,
): Effect.Effect<DownloadReporter, never, HostClient | Scope.Scope> =>
	Effect.gen(function* () {
		const host = yield* HostClient;
		const send = (rows: ReadonlyArray<DownloadRow>) =>
			Effect.runPromise(
				host
					.request("PUT", `/library/provider/${provider}/downloads`, {
						downloads: rows.map((r) => ({
							external_id: r.externalId,
							state: r.state,
							done_bytes: Math.max(0, Math.floor(r.doneBytes)),
							...(r.totalBytes !== undefined
								? { total_bytes: Math.max(0, Math.floor(r.totalBytes)) }
								: {}),
							...(r.phase ? { phase: r.phase } : {}),
							...(r.error ? { error: r.error } : {}),
						})),
					})
					.pipe(
						Effect.as("ok" as SendResult),
						Effect.catch((e) =>
							Effect.succeed<SendResult>(
								statusOf(e) === 404 ? "unsupported" : "failed",
							),
						),
					),
			);
		return yield* Effect.acquireRelease(
			Effect.sync(() => makeDownloadReporter(send)),
			(r) => Effect.sync(() => r.close()),
		);
	});
