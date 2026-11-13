import { expect, test } from "bun:test";
import { digitsOnly, isImeEvent } from "./ime";

const key = (isComposing: boolean, keyCode: number) => ({
	nativeEvent: { isComposing },
	keyCode,
});

test("a key during composition belongs to the IME", () => {
	expect(isImeEvent(key(true, 13))).toBe(true);
});

test("Safari's Enter after compositionend still reads as the IME's", () => {
	expect(isImeEvent(key(false, 229))).toBe(true);
});

test("a plain Enter is the player's", () => {
	expect(isImeEvent(key(false, 13))).toBe(false);
});

test("full-width digits become ASCII and everything else drops", () => {
	expect(digitsOnly("１２３４")).toBe("1234");
	expect(digitsOnly("12３4")).toBe("1234");
	expect(digitsOnly(" 12-34 ")).toBe("1234");
	expect(digitsOnly("ａｂｃ")).toBe("");
	expect(digitsOnly("")).toBe("");
});
