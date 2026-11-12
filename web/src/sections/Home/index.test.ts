import { expect, test } from "bun:test";
import type { ActiveGame } from "@/api/gen/model/activeGame";
import type { RuntimeStatus } from "@/api/gen/model/runtimeStatus";
import type { SessionRow } from "@/api/gen/model/sessionRow";
import { gameOf } from "./index";

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
