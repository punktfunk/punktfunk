// Units for the Performance charts. The host records microseconds; a person reads
// milliseconds, against the one frame the stream has to fit in.
import { fmtNumber } from "@/lib/format";

/** µs as a person reads it: `8.3 ms` from a millisecond up, `450 µs` below. */
export function dur(us: number): string {
	if (!Number.isFinite(us)) return "—";
	if (us >= 1000) return `${fmtNumber(us / 1000, 1)} ms`;
	return `${fmtNumber(Math.round(us))} µs`;
}

/** One frame at `fps`, in ms. `0` when the rate is unknown. */
export function frameBudgetMs(fps: number): number {
	return fps > 0 ? 1000 / fps : 0;
}

/** Top of a latency axis: the data, or the frame budget plus headroom, whichever is higher. */
export function latencyCeilingMs(dataMaxMs: number, budgetMs: number): number {
	const top = Math.max(
		Number.isFinite(dataMaxMs) ? dataMaxMs : 0,
		budgetMs * 1.2,
	);
	return top > 0 ? top : 1;
}

/** A millisecond tick: one decimal under 10 ms, whole numbers above. */
export function msTick(ms: number): string {
	return fmtNumber(ms, ms > 0 && ms < 10 ? 1 : 0);
}
