import { expect, test } from "bun:test";
import { describe as describeEntry } from "./index";

test("a game row names the client and the game", () => {
	const line = describeEntry({
		seq: 1,
		ts_ms: 0,
		kind: "game.running",
		data: {
			kind: "game.running",
			game: { client: "Deck", title: "Celeste", plane: "native" },
		},
	});
	expect(line).toBe("Deck · Celeste");
});

test("a local game's row names the game and no device", () => {
	const line = describeEntry({
		seq: 3,
		ts_ms: 0,
		kind: "game.running",
		data: {
			kind: "game.running",
			game: { client: "desk", title: "Dota 2", plane: "local" },
		},
	});
	expect(line).toBe("Dota 2");
});

test("an access row names the device", () => {
	const line = describeEntry({
		seq: 2,
		ts_ms: 0,
		kind: "access.expired",
		data: { kind: "access.expired", device: { name: "Guest phone" } },
	});
	expect(line).toBe("Guest phone");
});
