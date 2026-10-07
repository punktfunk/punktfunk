// The runner's log door into the web console (field report 2026-08-03, the VirtualHere plugin).
//
// WHY THIS EXISTS: plugins are not host child processes. The runner is a separate bun process that
// `import()`s each plugin in-process, so a plugin's output is THIS process's stdout and the host's
// `tracing` ring — the thing `GET /api/v1/logs` and the console's Logs page serve — never sees a
// byte of it. On Linux the fallback was `journalctl --user -u punktfunk-scripting`; on Windows the
// runner scheduled task wrote no log file at all (it does now — `scripting-run.cmd` tees to
// `%ProgramData%\punktfunk\plugin-state\runner.log`, because THIS door needs the very connection
// whose failure you'd be reading about; field report 2026-08-18). Both need shell access on the
// host box, which is the exact thing the console exists to avoid. A user hitting a plugin
// misconfiguration therefore had no way to see the error explaining it.
//
// So: tee every console line to `POST /api/v1/plugins/logs`, which lands it in the host's ring
// alongside the host's own lines under the target `plugin:<source>`.
//
// Design rules this file will not break:
// - **stdout stays authoritative.** The original console method is called FIRST and always, so
//   journald/foreground output is unchanged whatever the host is doing. Shipping is additive.
// - **Never recurse.** Nothing on the shipping path may log through the patched console; a failed
//   POST that logged its own failure would enqueue that line, fail again, and spin.
// - **Never throw into a caller.** `console.log` is not allowed to fail because the host is down.
// - **Bounded.** The queue has a hard cap and drops its OLDEST lines, then says how many — an
//   unreachable host must not turn the runner into a memory leak.
import { format } from "node:util";
import { type ConnectOptions, resolveConfig } from "./config.js";

/** The level a console method implies when a line carries no level of its own. */
const LEVEL_BY_METHOD = {
	log: "INFO",
	info: "INFO",
	debug: "DEBUG",
	warn: "WARN",
	error: "ERROR",
} as const;
type Method = keyof typeof LEVEL_BY_METHOD;
const METHODS = Object.keys(LEVEL_BY_METHOD) as Method[];

/**
 * `<ISO> [<source>] [LEVEL:] <message>` — the line format `plugin-kit`'s `loggingLayer` and this
 * package's `runner.ts` both emit. Parsing it back recovers the plugin name and level that the
 * formatting flattened, so the console can show `plugin:virtualhere` / `WARN` instead of one
 * undifferentiated `runner` stream.
 *
 * A line that does NOT match is still shipped — attributed to `runner` at the console method's own
 * level. A plugin calling bare `console.error("boom")` is precisely the case this must not lose.
 */
const STAMPED =
	/^(\d{4}-\d{2}-\d{2}T[\d:.]+Z) \[([^\]\n]{1,64})\](?:[ \t]+([A-Z]{3,9}):)?[ \t]?([\s\S]*)$/;

/** Lines per POST. Must stay ≤ the host's `MAX_LOG_BATCH` or a batch is rejected wholesale. */
const BATCH = 256;
/** Queued lines before the oldest start dropping. ~2 MB worst case at the host's 2 KB cap. */
const QUEUE_LIMIT = 1000;
/** A multi-line message (a stack trace) ships as one entry per line, capped here. */
const MAX_LINES_PER_MESSAGE = 40;

export interface LogShipperOptions {
	/** Connection overrides. Defaults to the same zero-config resolution `connect()` uses. */
	connect?: ConnectOptions;
	/** Flush cadence in ms (default 2000). */
	intervalMs?: number;
}

export interface LogShipper {
	/** Send whatever is queued right now. Never rejects. */
	flush: () => Promise<void>;
	/** Restore the original console methods and stop the timer. */
	stop: () => void;
}

interface Line {
	ts_ms: number;
	level: string;
	source: string;
	msg: string;
}

/**
 * Split a rendered console call into shippable lines.
 *
 * Newlines become separate entries rather than one blob: the log viewer is line-oriented, and the
 * payload that matters most here — an Effect `Cause.pretty` stack from a plugin that failed to
 * start — is unreadable folded onto a single row. The cap keeps one pathological dump from
 * evicting the ring on its own.
 */
const toLines = (method: Method, args: unknown[]): Line[] => {
	const text = format(...args);
	const m = STAMPED.exec(text);
	const source = m?.[2] ?? "runner";
	const level = m?.[3] ?? LEVEL_BY_METHOD[method];
	const parsedTs = m?.[1] !== undefined ? Date.parse(m[1]) : Number.NaN;
	const ts_ms = Number.isNaN(parsedTs) ? Date.now() : parsedTs;
	const body = m?.[4] ?? text;

	const parts = body.split(/\r?\n/);
	const kept = parts.slice(0, MAX_LINES_PER_MESSAGE);
	if (parts.length > kept.length) {
		kept.push(`… ${parts.length - kept.length} more line(s) suppressed`);
	}
	// A trailing newline yields one empty part; an all-empty message still ships one entry so the
	// console never silently swallows a call.
	const meaningful = kept.filter((l) => l.trim() !== "");
	return (meaningful.length > 0 ? meaningful : [""]).map((msg) => ({
		ts_ms,
		level,
		source,
		msg,
	}));
};

/**
 * Patch the console to tee into the host's log ring, and start the flush timer.
 *
 * Install this ONLY in the managed runner (`runner-cli.ts`). A plugin's own CLI (`punktfunk-plugin-x
 * doctor`) must keep its output local — an operator running a diagnostic in their terminal is not
 * asking to write to the host's log.
 */
export const installLogShipper = (
	options: LogShipperOptions = {},
): LogShipper => {
	const queue: Line[] = [];
	let droppedByOverflow = 0;
	// True from the moment a flush claims a batch until its POST settles. Guards flush RE-ENTRY
	// only — the interval can fire while a slow POST is still open, and two concurrent flushes
	// would splice disjoint batches out of one queue and deliver them out of order.
	//
	// It deliberately does NOT gate `enqueue`. It used to, as a recursion guard, and that silently
	// dropped every line logged during the few ms of a POST — which under load is a great many of
	// them, and exactly the lines a busy plugin is producing. The recursion it was guarding is
	// handled by the rule at the top of this file instead: nothing on the shipping path logs.
	let shipping = false;
	let stopped = false;
	// Set once the host's URL/token/CA resolve. Before that (the host writes `plugin-token` as it
	// boots, and the runner may well start first) lines keep queueing — those earliest lines are
	// exactly the ones that explain a plugin failing to load.
	let resolved: Awaited<ReturnType<typeof resolveConfig>> | undefined;
	let resolving = false;
	// Consecutive failures, for backoff. The host being down is normal (restart, update) and must
	// not mean a POST attempt every 2 s forever.
	let failures = 0;
	let skipTicks = 0;

	// The ORIGINAL function objects, unbound. `stop()` must put back exactly what it found: binding
	// here and restoring the bound copy would leave a different function in place each cycle, so
	// an install/stop/install sequence accumulates a wrapper per round. Calls go through `.call`
	// below to keep `this` right without touching identity.
	const original = Object.fromEntries(METHODS.map((m) => [m, console[m]])) as Record<
		Method,
		(...args: unknown[]) => void
	>;

	const enqueue = (method: Method, args: unknown[]) => {
		if (stopped) return;
		try {
			for (const line of toLines(method, args)) {
				if (queue.length >= QUEUE_LIMIT) {
					queue.shift();
					droppedByOverflow += 1;
				}
				queue.push(line);
			}
		} catch {
			// Formatting a hostile object must never break the caller's console call.
		}
	};

	for (const method of METHODS) {
		console[method] = ((...args: unknown[]) => {
			original[method].call(console, ...args);
			enqueue(method, args);
		}) as typeof console.log;
	}

	const ensureResolved = async (): Promise<boolean> => {
		if (resolved) return true;
		if (resolving) return false;
		resolving = true;
		try {
			resolved = await resolveConfig(options.connect);
			return true;
		} catch {
			// No token yet (or no host at all). Keep buffering and try again next tick.
			return false;
		} finally {
			resolving = false;
		}
	};

	/** Put a failed batch back at the FRONT, still honoring the cap (oldest lose). */
	const requeue = (batch: Line[]) => {
		queue.unshift(...batch);
		if (queue.length > QUEUE_LIMIT) {
			droppedByOverflow += queue.length - QUEUE_LIMIT;
			queue.splice(0, queue.length - QUEUE_LIMIT);
		}
	};

	const flush = async (): Promise<void> => {
		if (stopped || shipping || queue.length === 0) return;
		if (!(await ensureResolved()) || !resolved) return;
		// Re-check after the await: `ensureResolved` yields, so another flush may have claimed the
		// queue in the meantime.
		if (stopped || shipping || queue.length === 0) return;

		shipping = true;
		const batch = queue.splice(0, BATCH);
		if (droppedByOverflow > 0) {
			// Tell the operator the tail is incomplete rather than presenting a gap as continuity —
			// the same contract the host's ring keeps with its `dropped` flag.
			batch.unshift({
				ts_ms: Date.now(),
				level: "WARN",
				source: "runner",
				msg: `log shipper dropped ${droppedByOverflow} line(s): the queue filled while the host was unreachable`,
			});
			droppedByOverflow = 0;
		}

		try {
			const headers: Record<string, string> = { "content-type": "application/json" };
			const auth = await resolved.credential.header();
			if (auth) headers.authorization = auth;
			const res = await resolved.fetch(`${resolved.url}/api/v1/plugins/logs`, {
				method: "POST",
				headers,
				body: JSON.stringify({ entries: batch }),
			});
			if (!res.ok) {
				// 4xx is our bug (a shape the host rejects) and retrying cannot fix it — drop the
				// batch. 5xx/transport is the host's problem and worth keeping.
				if (res.status >= 500) requeue(batch);
				// A token that went stale (host re-keyed) resolves again from disk on the next tick.
				if (res.status === 401) resolved = undefined;
				failures += 1;
			} else {
				failures = 0;
			}
		} catch {
			requeue(batch);
			failures += 1;
		} finally {
			shipping = false;
			// 2 s, 4 s, 8 s … capped at ~30 s while the host stays away.
			skipTicks = failures === 0 ? 0 : Math.min(2 ** (failures - 1), 15);
		}
	};

	// The most recent flush, so an explicit `flush()` can WAIT for a periodic one rather than hit
	// the re-entry guard and return having sent nothing. That matters on the shutdown path: the
	// runner flushes once more after its units' finalizers have run, and those last lines are the
	// ones that say whether the shutdown was clean. The window is widest exactly when the host is
	// slow — which is when the logs are worth most.
	let inFlight: Promise<void> = Promise.resolve();
	const runFlush = (): Promise<void> => {
		inFlight = flush();
		return inFlight;
	};

	const timer = setInterval(() => {
		if (skipTicks > 0) {
			skipTicks -= 1;
			return;
		}
		void runFlush();
	}, options.intervalMs ?? 2_000);
	// The runner parks on its own keep-alive handle; this timer must not be what holds the process
	// open, or a runner with nothing to run would never exit.
	timer.unref?.();

	return {
		flush: async () => {
			skipTicks = 0;
			// `flush` never rejects (every path is caught); `.catch` only keeps that a guarantee.
			await inFlight.catch(() => {});
			await runFlush();
		},
		stop: () => {
			stopped = true;
			clearInterval(timer);
			for (const method of METHODS) {
				console[method] = original[method] as typeof console.log;
			}
		},
	};
};

/** Exported for tests. */
export const __test = { toLines, STAMPED };
