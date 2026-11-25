// A number row saves by itself once the player stops, once per value, and never loses an edit
// to the host's answer for an earlier one.
import { afterEach, beforeEach, expect, jest, test } from "bun:test";
import { saveOnPause } from "./saveOnPause";

const WAIT = 600;

beforeEach(() => jest.useFakeTimers());
afterEach(() => jest.useRealTimers());

/** A field whose host holds 0, and the values it has sent. */
function field() {
	const sent: number[] = [];
	const saver = saveOnPause<number>(WAIT, 0);
	const edit = (n: number) => saver.change(n, (v) => sent.push(v));
	return { sent, saver, edit };
}

test("an edit saves once after the pause, with no blur", () => {
	const { sent, edit } = field();
	edit(1);
	jest.advanceTimersByTime(WAIT - 1);
	expect(sent).toEqual([]);
	jest.advanceTimersByTime(1);
	expect(sent).toEqual([1]);
	jest.advanceTimersByTime(10 * WAIT);
	expect(sent).toEqual([1]);
});

test("each edit restarts the wait, and only the last value goes", () => {
	const { sent, edit } = field();
	edit(1);
	jest.advanceTimersByTime(WAIT - 100);
	edit(2);
	jest.advanceTimersByTime(WAIT - 100);
	expect(sent).toEqual([]);
	jest.advanceTimersByTime(100);
	expect(sent).toEqual([2]);
});

test("a blur saves at once, and neither the timer nor a second blur sends it again", () => {
	const { sent, saver, edit } = field();
	edit(1);
	saver.flush();
	expect(sent).toEqual([1]);
	saver.flush();
	jest.advanceTimersByTime(10 * WAIT);
	expect(sent).toEqual([1]);
});

test("a blur with no edit sends nothing", () => {
	const { sent, saver } = field();
	saver.flush();
	expect(sent).toEqual([]);
});

test("an edit back to what the host holds sends nothing", () => {
	const { sent, saver, edit } = field();
	edit(1);
	edit(0);
	jest.advanceTimersByTime(WAIT);
	saver.flush();
	expect(sent).toEqual([]);
});

test("the host's answer leaves a waiting edit alone and does not resend it", () => {
	const { sent, saver, edit } = field();
	edit(2);
	expect(saver.follow(1)).toBe(false);
	expect(sent).toEqual([]);
	jest.advanceTimersByTime(WAIT);
	expect(sent).toEqual([2]);
});

test("a waiting edit that equals the host's new value is not sent", () => {
	const { sent, saver, edit } = field();
	edit(2);
	saver.follow(2);
	jest.advanceTimersByTime(WAIT);
	expect(sent).toEqual([]);
});

test("the answer to a save is shown, once no edit waits", () => {
	const { saver, edit } = field();
	edit(2);
	jest.advanceTimersByTime(WAIT);
	expect(saver.follow(2)).toBe(true);
});
