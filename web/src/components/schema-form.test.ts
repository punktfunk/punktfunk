import { describe, expect, test } from "bun:test";
import { toLines, unquote } from "./schema-form";

describe("handed path input", () => {
	test("a path pasted from Explorer loses its quotes", () => {
		const pasted = String.raw`"C:\Users\me\Documents\Game Saves\Game.ini"`;
		expect(unquote(pasted)).toBe(
			String.raw`C:\Users\me\Documents\Game Saves\Game.ini`,
		);
		expect(toLines(`${pasted}\n  "/home/me/.config/game"  \n\n`, true)).toEqual(
			[
				String.raw`C:\Users\me\Documents\Game Saves\Game.ini`,
				"/home/me/.config/game",
			],
		);
	});

	test("only quotes around the whole path come off", () => {
		expect(unquote(`/games/"live"/save`)).toBe(`/games/"live"/save`);
		expect(unquote("/games/Assassin's Creed")).toBe("/games/Assassin's Creed");
	});

	test("a list that is not paths keeps its quotes", () => {
		expect(toLines(`"--fullscreen"\n`, false)).toEqual([`"--fullscreen"`]);
	});
});
