import { describe, expect, test } from "bun:test";
import { gamePlugins, type PluginSummary, uiPlugins } from "./plugins";

const p = (id: string, ui?: PluginSummary["ui"], category?: string) => ({
	id,
	title: id,
	ui,
	category,
});

describe("plugin surfaces", () => {
	test("a page is assumed when an older host sends no flag", () => {
		const list = [
			p("old", { port: 1 }),
			p("tab-only", { port: 2, page: false, game: true }),
			p("scanner", { port: 3 }, "library"),
			// An older SDK sends no `page` flag; the category still keeps a source out of the nav.
			p("covers", { port: 4 }, "metadata"),
			p("headless"),
		];
		expect(uiPlugins(list).map((x) => x.id)).toEqual(["old"]);
		expect(gamePlugins(list).map((x) => x.id)).toEqual(["tab-only"]);
	});
});
