import { expect, test } from "bun:test";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import type { PluginAccessSnapshot } from "@/api/gen/model/pluginAccessSnapshot";
import { baseLocale, overwriteGetLocale } from "@/paraglide/runtime";
import { nextAsk, PendingAccess, putOff } from "./PluginAccess";

overwriteGetLocale(() => baseLocale);

const row = (plugin: string, ...paths: string[]) =>
	({
		plugin,
		grants: [],
		denied: [],
		pending: paths.map((path) => ({ path, write: false })),
	}) as unknown as PluginAccessSnapshot;

test("the prompt asks for a request until Later puts it off, then the next", () => {
	const rows = [row("steam", "/mnt/a", "/mnt/b"), row("gog", "/games")];
	expect(nextAsk(rows, [])?.plugin).toBe("steam");
	const later = putOff(rows, [], rows[0]);
	expect(later).toEqual(["steam\n/mnt/a", "steam\n/mnt/b"]);
	expect(nextAsk(rows, later)?.plugin).toBe("gog");
	// A new folder from the same plugin asks again.
	expect(nextAsk([row("steam", "/mnt/a", "/mnt/c")], later)?.plugin).toBe(
		"steam",
	);
});

test("Later forgets requests that are no longer pending", () => {
	const rows = [row("gog", "/games")];
	expect(putOff(rows, ["steam\n/mnt/a"], rows[0])).toEqual(["gog\n/games"]);
});

test("a request through a link names the folder it opens first, the link below", () => {
	const target = "/home/u/.local/share/punktfunk/emulators/x/app";
	const link = "/home/u/.local/share/punktfunk/plugins/p/state/x";
	const access = {
		plugin: "p",
		grants: [],
		denied: [],
		pending: [{ path: target, spelled: link, write: true }],
	} as unknown as PluginAccessSnapshot;
	const html = renderToStaticMarkup(
		createElement(
			QueryClientProvider,
			{ client: new QueryClient() },
			createElement(PendingAccess, {
				access,
				busy: false,
				onDecide: () => {},
			}),
		),
	);
	expect(html).toContain(`>${target}</div>`);
	expect(html).toContain(`Asked for through the link ${link}`);
	expect(html.indexOf(target)).toBeLessThan(html.indexOf(link));
});
