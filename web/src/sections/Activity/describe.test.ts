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

test("a session row names the device, not its key", () => {
	const line = describeEntry(
		{
			seq: 4,
			ts_ms: 0,
			kind: "session.ended",
			data: {
				kind: "session.ended",
				session: { client: "417075f54a8b", fingerprint: "417075f54a8bdb86" },
			},
		},
		{ device: (fp) => (fp === "417075f54a8bdb86" ? "AYN Thor" : undefined) },
	);
	expect(line).toBe("AYN Thor");
});

test("a native stream names its game by the library; a GameStream app is its title", () => {
	const games = {
		game: (id: string) => (id === "steam:570" ? "Dota 2" : undefined),
	};
	const stream = (plane: string, app: string) =>
		describeEntry(
			{
				seq: 5,
				ts_ms: 0,
				kind: "stream.stopped",
				data: { kind: "stream.stopped", stream: { app, plane, client: "x" } },
			},
			games,
		);
	expect(stream("native", "steam:570")).toBe("Dota 2");
	expect(stream("native", "custom:bc97c03ff182")).toBe("");
	expect(stream("gamestream", "Desktop")).toBe("Desktop");
});
