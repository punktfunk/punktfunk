// The SSE half of the SDK: a spec-shaped parser, one shared reconnecting frame source and one
// frame classifier both surfaces consume — the Effect `Stream` wraps them, the Promise facade
// iterates them directly, so there is exactly one implementation of connect/parse/resume/decode
// (RFC §7: "two surfaces, one core"). Reconnects carry `Last-Event-ID` (the host replays from its
// ring); backoff is exponential + jittered, capped, and resets after a healthy connection.
import { Result } from "effect";
import type { Connection } from "./connection.js";
import { decodeHostEvent, type HostEvent } from "./wire.js";

/** One parsed SSE frame. */
export interface SseFrame {
	/** The `event:` name — an event kind, or `dropped`, or `message` when absent. */
	event: string;
	/** The joined `data:` payload. */
	data: string;
	/** The `id:` field (the host sets it to the event's `seq`). */
	id?: string;
}

/**
 * What one frame is. `dropped` is the fell-off-the-ring marker, `live` the end of the host's
 * catch-up, `garbled` a payload that is not JSON, `unknown` a kind this SDK cannot decode (a newer
 * host). The surfaces differ only in how they react.
 */
export type ClassifiedFrame =
	| { tag: "event"; event: HostEvent }
	| { tag: "unknown"; json: unknown }
	| { tag: "dropped" | "live" | "garbled" };

export function classifyFrame(frame: SseFrame): ClassifiedFrame {
	if (frame.event === "dropped") return { tag: "dropped" };
	if (frame.event === "live") return { tag: "live" };
	let json: unknown;
	try {
		json = JSON.parse(frame.data);
	} catch {
		return { tag: "garbled" };
	}
	const decoded = decodeHostEvent(json);
	return Result.isFailure(decoded)
		? { tag: "unknown", json }
		: { tag: "event", event: decoded.success };
}

/** Incremental SSE parser (the WHATWG dispatch rules the host's frames need). */
export class SseParser {
	private buf = "";
	private event = "";
	private data: string[] = [];
	private id: string | undefined;

	/** Feed a chunk; returns the frames it completed. */
	push(chunk: string): SseFrame[] {
		this.buf += chunk;
		const frames: SseFrame[] = [];
		for (;;) {
			const nl = this.buf.indexOf("\n");
			if (nl < 0) break;
			let line = this.buf.slice(0, nl);
			this.buf = this.buf.slice(nl + 1);
			if (line.endsWith("\r")) line = line.slice(0, -1);
			if (line.length === 0) {
				// Blank line = dispatch.
				if (this.data.length > 0 || this.event.length > 0) {
					frames.push({
						event: this.event.length > 0 ? this.event : "message",
						data: this.data.join("\n"),
						id: this.id,
					});
				}
				this.event = "";
				this.data = [];
				continue;
			}
			if (line.startsWith(":")) continue; // comment (the host's keep-alives)
			const colon = line.indexOf(":");
			const field = colon < 0 ? line : line.slice(0, colon);
			let value = colon < 0 ? "" : line.slice(colon + 1);
			if (value.startsWith(" ")) value = value.slice(1);
			switch (field) {
				case "event":
					this.event = value;
					break;
				case "data":
					this.data.push(value);
					break;
				case "id":
					this.id = value;
					break;
				default: // unknown fields are ignored per spec (retry: handled by our own backoff)
			}
		}
		return frames;
	}
}

export interface EventStreamOptions {
	/**
	 * Resume cursor (`?since=`); reconnects use the newer `Last-Event-ID` automatically.
	 * **Omitted = live tail only** (events from now on — a fresh notify script must not
	 * re-fire on the host's replayed history); `0` = replay the host's full ring first;
	 * `N` = resume after seq N.
	 */
	since?: number;
	/** Server-side kind filter (`["stream.*", "pairing.pending"]`). */
	kinds?: string[];
	/** Called on transient trouble (reconnects, decode warnings). Default: console.warn. */
	onWarning?: (message: string) => void;
	/** Ends the stream, even mid-request or mid-backoff, where `return()` cannot reach it. */
	signal?: AbortSignal;
}

/**
 * The "live tail only" cursor: beyond any realistic seq, so the host's catch-up is empty and
 * (being > 0 yet ≥ the ring head) it never trips the `dropped` marker. The first received
 * frame's real id replaces it for reconnects.
 */
const LIVE_ONLY_CURSOR = Number.MAX_SAFE_INTEGER;

/** Thrown when the stream cannot ever work (bad credentials) — retrying would loop 401s. */
export class SseAuthError extends Error {
	readonly _tag = "SseAuthError";
}

const BACKOFF_INITIAL_MS = 500;
const BACKOFF_CAP_MS = 15_000;
/** A connection that lived this long resets the backoff (it was healthy, not flapping). */
const HEALTHY_MS = 30_000;

/** Ends after `ms`, or at once when `signal` aborts. */
const sleep = (ms: number, signal: AbortSignal) =>
	new Promise<void>((resolve) => {
		const done = () => {
			clearTimeout(timer);
			signal.removeEventListener("abort", done);
			resolve();
		};
		const timer = setTimeout(done, ms);
		signal.addEventListener("abort", done, { once: true });
	});

/**
 * The shared frame source: connect → parse → yield frames, forever — reconnecting with
 * `Last-Event-ID` on any hiccup. Ends on `opts.signal`, on a consumer break/return, or throws
 * [`SseAuthError`]. A `return()` waits for the next frame, so a caller that must stop while
 * the host is gone passes `signal`.
 */
export async function* sseFrames(
	cfg: Connection,
	opts: EventStreamOptions = {},
): AsyncGenerator<SseFrame> {
	const warn = opts.onWarning ?? ((m) => console.warn(`[punktfunk] ${m}`));
	let lastId: number = opts.since ?? LIVE_ONLY_CURSOR;
	let backoff = BACKOFF_INITIAL_MS;
	const abort = new AbortController();
	const stop = () => abort.abort();
	if (opts.signal?.aborted) stop();
	opts.signal?.addEventListener("abort", stop, { once: true });
	try {
		while (!abort.signal.aborted) {
			const url = new URL(`${cfg.url}/api/v1/events`);
			if (opts.kinds && opts.kinds.length > 0)
				url.searchParams.set("kinds", opts.kinds.join(","));
			const headers: Record<string, string> = { accept: "text/event-stream" };
			const auth = await cfg.credential.header();
			if (auth) headers.authorization = auth;
			if (lastId !== 0) headers["last-event-id"] = String(lastId);

			const connectedAt = Date.now();
			try {
				const resp = await cfg.fetch(url, {
					headers,
					signal: abort.signal,
				});
				if (resp.status === 401) throw new SseAuthError("invalid credentials");
				if (!resp.ok || !resp.body)
					throw new Error(`event stream HTTP ${resp.status}`);
				const reader = resp.body.getReader();
				const decoder = new TextDecoder();
				const parser = new SseParser();
				try {
					for (;;) {
						const { done, value } = await reader.read();
						if (done) break; // server closed (slow-consumer cut / shutdown) → reconnect
						for (const frame of parser.push(
							decoder.decode(value, { stream: true }),
						)) {
							if (frame.id !== undefined) {
								const id = Number(frame.id);
								if (Number.isFinite(id)) lastId = id;
							}
							yield frame;
						}
					}
				} finally {
					reader.cancel().catch(() => {});
				}
				throw new Error("event stream ended");
			} catch (e) {
				if (e instanceof SseAuthError) throw e;
				if (abort.signal.aborted) return;
				if (Date.now() - connectedAt >= HEALTHY_MS) backoff = BACKOFF_INITIAL_MS;
				const jittered = backoff * (0.5 + Math.random());
				warn(
					`event stream reconnecting in ${Math.round(jittered)}ms (${
						e instanceof Error ? e.message : String(e)
					})`,
				);
				await sleep(jittered, abort.signal);
				backoff = Math.min(backoff * 2, BACKOFF_CAP_MS);
			}
		}
	} finally {
		opts.signal?.removeEventListener("abort", stop);
		abort.abort(); // consumer went away — tear down any in-flight request
	}
}
