// A game only one profile has lives on that profile's desktop: its host answers only while the
// desktop runs. A call for it starts a stopped desktop and waits, as Profiles' Start does.
import { useCallback, useRef, useState } from "react";
import { inSeat } from "@/api/fetcher";
import type { SeatPublic } from "@/api/gen/model/seatPublic";
import { listProfiles, startProfileSeat } from "@/api/gen/profiles/profiles";
import { apiErrorCode } from "@/lib/errors";
import { seatDetail } from "@/lib/host-messages";
import { personBrowser } from "@/lib/person";
import { m } from "@/paraglide/messages";

/** A cold desktop takes about 20 s; past this it counts as not starting. */
const LIMIT_MS = 180_000;
/** A desktop still stopped this long after its start never began starting. */
const STUCK_MS = 20_000;
const POLL_MS = 1_000;

/** Answers that mean the call never reached the desktop's host, so sending it again is safe. */
const ASLEEP = new Set(["seat_no_answer_start", "owner_session_starting"]);

/** Why a call that needed a profile's desktop didn't run, as one sentence for the person. */
export class DesktopError extends Error {
	override toString(): string {
		return this.message;
	}
}

/** A wait the person cancelled: nothing to report. */
export const isAbort = (e: unknown): boolean =>
	e instanceof Error && e.name === "AbortError";

/** What `onDesktop` needs from the box; tests hand in their own. */
export interface DesktopDeps {
	/** The profile's desktop as the box lists it now; absent when it has none. */
	seat: (signal: AbortSignal) => Promise<SeatPublic | null | undefined>;
	start: () => Promise<unknown>;
	sleep: (ms: number, signal: AbortSignal) => Promise<void>;
	now: () => number;
}

const sleep = (ms: number, signal: AbortSignal) =>
	new Promise<void>((done, fail) => {
		const t = setTimeout(done, ms);
		signal.addEventListener(
			"abort",
			() => {
				clearTimeout(t);
				fail(signal.reason);
			},
			{ once: true },
		);
	});

/**
 * The box's answers for profile `id`, whatever cache the caller runs in. In a person's own
 * console their desktop is the one they are using: nothing to start, every call goes at once.
 */
export const liveDeps = (id: string): DesktopDeps =>
	personBrowser()
		? { seat: async () => null, start: async () => {}, sleep, now: Date.now }
		: {
				seat: (signal) =>
					inSeat(null, () => listProfiles({ signal })).then(
						(list) => list.find((p) => p.id === id)?.seat,
					),
				start: () => inSeat(null, () => startProfileSeat(id)),
				sleep,
				now: Date.now,
			};

export interface DesktopCall<T> {
	/** The profile's name, for the sentence when its desktop doesn't start. */
	name: string;
	/** Sent once the desktop is up, never after `signal` aborts. */
	call: () => Promise<T>;
	signal: AbortSignal;
	/** True when the call starts waiting for the desktop, false when it is sent. */
	onWait: (waiting: boolean) => void;
	deps: DesktopDeps;
}

/**
 * `call` once the profile's desktop is up. A stopped desktop is started. One that fails, falls
 * back to stopped, or isn't up within `LIMIT_MS` rejects with a `DesktopError`; an unavailable
 * one with the host's own sentence. An answer that never reached its host is sent again.
 */
export async function onDesktop<T>({
	name,
	call,
	signal,
	onWait,
	deps,
}: DesktopCall<T>): Promise<T> {
	const end = deps.now() + LIMIT_MS;
	const failed = () =>
		new DesktopError(m.profile_view_desktop_not_ready({ name }));
	let waiting = false;
	const wait = (w: boolean) => {
		if (w === waiting) return;
		waiting = w;
		onWait(w);
	};
	let started: number | null = null;
	let seenStarting = false;
	for (;;) {
		signal.throwIfAborted();
		const seat = await deps.seat(signal);
		const state = seat?.state;
		if (!seat || state === "ready" || state === "occupied") {
			signal.throwIfAborted();
			wait(false);
			try {
				return await call();
			} catch (e) {
				if (!ASLEEP.has(apiErrorCode(e) ?? "")) throw e;
			}
		} else if (state === "unavailable") {
			throw new DesktopError(seatDetail(seat) || failed().message);
		} else if (state === "starting") {
			seenStarting = true;
		} else if (seenStarting) {
			throw failed();
		} else if (started === null) {
			await deps.start();
			started = deps.now();
		} else if (deps.now() - started > STUCK_MS) {
			throw failed();
		}
		wait(true);
		if (deps.now() > end) throw failed();
		await deps.sleep(POLL_MS, signal);
	}
}

/**
 * Calls on `profile`'s own desktop. `run` sends one through `onDesktop`; its `onWait` gets that
 * run's cancel when it starts waiting. `waiting` is true while any run waits, and `cancel` drops
 * every wait before anything is sent. A run outlives the component; a caller that must not
 * cancels on unmount.
 */
export function useDesktop(profile: { id: string; name: string } | null) {
	const [waits, setWaits] = useState(0);
	const live = useRef(new Set<AbortController>());
	const id = profile?.id;
	const name = profile?.name ?? "";
	const run = useCallback(
		async <T>(
			call: () => Promise<T>,
			onWait?: (cancel: () => void) => void,
		): Promise<T> => {
			if (!id) return call();
			const c = new AbortController();
			live.current.add(c);
			let counted = false;
			const count = (w: boolean) => {
				counted = w;
				setWaits((n) => n + (w ? 1 : -1));
			};
			try {
				return await onDesktop({
					name,
					call: () => inSeat(id, call),
					signal: c.signal,
					onWait: (w) => {
						count(w);
						if (w) onWait?.(() => c.abort());
					},
					deps: liveDeps(id),
				});
			} finally {
				live.current.delete(c);
				if (counted) count(false);
			}
		},
		[id, name],
	);
	const cancel = useCallback(() => {
		for (const c of live.current) c.abort();
	}, []);
	return { run, waiting: waits > 0, cancel };
}
