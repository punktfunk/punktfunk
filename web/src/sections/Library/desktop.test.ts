import { expect, test } from "bun:test";
import { ApiError } from "@/api/fetcher";
import type { SeatPublic } from "@/api/gen/model/seatPublic";
import type { SeatState } from "@/api/gen/model/seatState";
import { type DesktopDeps, DesktopError, isAbort, onDesktop } from "./desktop";

/** A box whose desktop reads `states` in turn, the last one for good, on a clock `sleep` moves. */
function box(states: (SeatState | SeatPublic)[]) {
	let t = 0;
	let read = 0;
	const log = { starts: 0, waits: [] as boolean[], calls: 0 };
	const deps: DesktopDeps = {
		seat: async () => {
			const s = states[Math.min(read++, states.length - 1)];
			return typeof s === "string" ? { state: s, port: 1, kind: "desktop" } : s;
		},
		start: async () => {
			log.starts++;
		},
		sleep: async (ms, signal) => {
			signal.throwIfAborted();
			t += ms;
		},
		now: () => t,
	};
	const run = <T>(
		call: () => Promise<T>,
		signal = new AbortController().signal,
	) =>
		onDesktop({
			name: "Kid",
			call: () => {
				log.calls++;
				return call();
			},
			signal,
			onWait: (w) => log.waits.push(w),
			deps,
		});
	return { run, log };
}

test("a desktop that runs takes the call at once", async () => {
	const { run, log } = box(["occupied"]);
	expect(await run(async () => "saved")).toBe("saved");
	expect(log).toEqual({ starts: 0, waits: [], calls: 1 });
});

test("a stopped desktop is started once, and the call goes out when it is up", async () => {
	const { run, log } = box([
		"stopped",
		"stopped",
		"starting",
		"starting",
		"ready",
	]);
	expect(await run(async () => "saved")).toBe("saved");
	expect(log).toEqual({ starts: 1, waits: [true, false], calls: 1 });
});

test("an unavailable desktop fails with the host's sentence, and nothing is sent", async () => {
	const why = "Remote Desktop licensing has run out for this server.";
	const { run, log } = box([
		{ state: "unavailable", port: 1, kind: "desktop", detail: why },
	]);
	const e = await run(async () => "saved").catch((e) => e);
	expect(e).toBeInstanceOf(DesktopError);
	expect(String(e)).toBe(why);
	expect(log.calls).toBe(0);
});

test("a desktop that stops again, or never starts, doesn't start", async () => {
	const fell = box(["stopped", "starting", "stopped"]);
	expect(await fell.run(async () => 1).catch(String)).toBe(
		"Kid's desktop didn't start. Try again in a moment.",
	);
	const stuck = box(["stopped"]);
	expect(await stuck.run(async () => 1).catch((e) => e)).toBeInstanceOf(
		DesktopError,
	);
	expect(stuck.log).toEqual({ starts: 1, waits: [true], calls: 0 });
});

test("a call that never reached the desktop's host goes out again; a refusal does not", async () => {
	const asleep = box(["ready"]);
	let tries = 0;
	const saved = await asleep.run(async () => {
		if (tries++ === 0)
			throw new ApiError(502, { code: "seat_no_answer_start", error: "x" });
		return "saved";
	});
	expect(saved).toBe("saved");
	expect(asleep.log.waits).toEqual([true, false]);

	const refused = box(["ready"]);
	const no = new ApiError(403, { code: "library_own_account", error: "x" });
	expect(await refused.run(() => Promise.reject(no)).catch((e) => e)).toBe(no);
	expect(refused.log.calls).toBe(1);
});

test("a cancelled wait sends nothing", async () => {
	const { run, log } = box(["stopped", "starting"]);
	const ctl = new AbortController();
	const done = run(async () => "saved", ctl.signal);
	ctl.abort();
	const e = await done.catch((e) => e);
	expect(isAbort(e)).toBe(true);
	expect(log.calls).toBe(0);
});
