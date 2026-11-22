import { expect, test } from "bun:test";
import type { ActiveGame } from "@/api/gen/model/activeGame";
import type { RuntimeStatus } from "@/api/gen/model/runtimeStatus";
import type { SessionRow } from "@/api/gen/model/sessionRow";
import { gameOf, unstreamed } from "./index";

const row = (seat: string, id: number) =>
	({ id, seat, plane: "native", client: "" }) as SessionRow;
const game = (seat: string, session_id: number, title: string) =>
	({
		seat,
		session_id,
		title,
		plane: "native",
		state: "running",
	}) as ActiveGame;

test("a door's game belongs to its own seat's session, though both seats count from 1", () => {
	const owner = row("owner", 1);
	const kid = row("kid", 1);
	const s = {
		sessions: [owner, kid],
		games: [game("owner", 1, "Hades"), game("kid", 1, "Celeste")],
	} as RuntimeStatus;
	expect(gameOf(s, owner)?.title).toBe("Hades");
	expect(gameOf(s, kid)?.title).toBe("Celeste");
});

const gamestream = { client: "Moonlight", plane: "gamestream" } as SessionRow;
const desk = (over: Partial<ActiveGame> = {}) =>
	({
		client: "desk",
		title: "Dota 2",
		plane: "local",
		state: "running",
		...over,
	}) as ActiveGame;
const compat = (over: Partial<ActiveGame> = {}) =>
	({
		client: "Moonlight",
		title: "Hades",
		plane: "gamestream",
		state: "running",
		...over,
	}) as ActiveGame;

test("a local game, which has no session, is no GameStream row's game", () => {
	const alone = { sessions: [gamestream], games: [desk()] } as RuntimeStatus;
	expect(gameOf(alone, gamestream)).toBeUndefined();
	const both = { sessions: [gamestream], games: [desk(), compat()] };
	expect(gameOf(both as RuntimeStatus, gamestream)?.title).toBe("Hades");
});

test("a local game is no session's game, whichever session is up", () => {
	const s = {
		sessions: [row("owner", 1), gamestream],
		games: [desk({ seat: "owner" })],
	} as RuntimeStatus;
	for (const r of s.sessions) expect(gameOf(s, r)).toBeUndefined();
});

test("a local game ends as a game nobody streams, not by stopping a session", () => {
	expect(unstreamed(desk())).toBe(true);
	expect(unstreamed(desk({ state: "launching" }))).toBe(true);
	expect(unstreamed(game("", 1, "Celeste"))).toBe(false);
	expect(unstreamed(compat())).toBe(false);
	expect(unstreamed(compat({ state: "grace" }))).toBe(true);
	expect(unstreamed(compat({ state: "detached" }))).toBe(true);
});
