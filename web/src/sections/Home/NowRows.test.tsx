// A game the host runs on its own screen reads as one, with no device behind it, and offers End
// only while the host can end it.
import { expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";
import type { ActiveGame } from "@/api/gen/model/activeGame";
import { baseLocale, overwriteGetLocale } from "@/paraglide/runtime";
import { GameRowView } from "./NowRows";

overwriteGetLocale(() => baseLocale);

const local = (over: Partial<ActiveGame> = {}): ActiveGame => ({
	client: "desk",
	app_id: "steam:570",
	title: "Dota 2",
	plane: "local",
	state: "running",
	endable: true,
	...over,
});

/** The row's text, tags dropped. */
const text = (game: ActiveGame) =>
	renderToStaticMarkup(
		<GameRowView game={game} onEnd={() => {}} isEnding={false} />,
	)
		.replace(/<[^>]*>/g, " ")
		.replace(/&#x27;/g, "'")
		.replace(/\s+/g, " ");

test("a local game is a title on this PC's screen, never a device's", () => {
	const row = text(local());
	expect(row).toContain("Dota 2");
	expect(row).toContain("Running");
	expect(row).toContain("On this PC's screen");
	expect(row).toContain("End now");
	expect(row).not.toContain("desk");
	expect(row).not.toContain("GameStream");
});

test("a local game with its window up still reads as running", () => {
	expect(text(local({ state: "window" }))).toContain("Running");
	expect(text(local({ state: "launching" }))).toContain("Starting");
});

test("a launch that died says why, and offers no End", () => {
	const row = text(
		local({
			state: "exited",
			endable: undefined,
			outcome: { code: "launch_download_failed", params: { title: "Dota 2" } },
		}),
	);
	expect(row).toContain("Ended");
	expect(row).toContain("Dota 2 didn't download.");
	expect(row).not.toContain("On this PC's screen");
	expect(row).not.toContain("End now");
});

test("an outcome this console has no words for leaves the plane's line", () => {
	const row = text(
		local({
			state: "exited",
			endable: undefined,
			outcome: { code: "launch_from_the_future", params: {} },
		}),
	);
	expect(row).toContain("On this PC's screen");
});

test("a GameStream game still names its client and its plane, and offers End", () => {
	const row = text({
		client: "Living room",
		title: "Hades",
		plane: "gamestream",
		state: "running",
	});
	expect(row).toContain("Living room · GameStream");
	expect(row).toContain("End now");
});
