import { expect, test } from "bun:test";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import type { PluginAccessSnapshot } from "@/api/gen/model/pluginAccessSnapshot";
import { baseLocale, overwriteGetLocale } from "@/paraglide/runtime";
import { accessLine } from "./Library/Sources";
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

const pending = (access: PluginAccessSnapshot) =>
	renderToStaticMarkup(
		createElement(
			QueryClientProvider,
			{ client: new QueryClient() },
			createElement(PendingAccess, { access, busy: false, onDecide: () => {} }),
		),
	);

test("an install shows its download while it runs, and its failure until the next try", () => {
	const app = "/home/u/.local/share/punktfunk/emulators/cemu/app";
	const asked = {
		plugin: "rom-manager",
		grants: [],
		denied: [],
		pending: [{ path: app, write: false, emulator: "cemu" }],
	} as unknown as PluginAccessSnapshot;
	const running = pending({
		...asked,
		walks: [
			{ path: app, removing: false, seconds: 3, installing: true, percent: 40 },
		],
	});
	expect(running).toContain("Downloading… 40 %");
	expect(running).not.toContain(">Install</button>");
	const sized = pending({
		...asked,
		walks: [{ path: app, removing: false, seconds: 3, installing: true }],
	});
	expect(sized).toContain("Installing… 3 s");
	const failed = pending({
		...asked,
		failed: [{ path: app, error: "GET https://x: timed out" }],
	});
	expect(failed).toContain(
		"cemu didn&#x27;t install — GET https://x: timed out",
	);
	expect(failed).toContain(">Install</button>");
});

test("several installs not yet running start on one yes", () => {
	const app = (id: string) => `/e/${id}/app`;
	const ask = (id: string) => ({ path: app(id), write: false, emulator: id });
	const asks = (...rows: object[]) =>
		({
			plugin: "rom-manager",
			grants: [],
			denied: [],
			pending: rows,
		}) as unknown as PluginAccessSnapshot;
	const two = asks(ask("cemu"), ask("dolphin"));
	expect(pending(two)).toContain(">Install all</button>");
	expect(pending(asks(ask("cemu")))).not.toContain("Install all");
	// A folder beside them is not part of it, and keeps Allow all away.
	const mixed = pending(asks(...two.pending, { path: "/roms", write: false }));
	expect(mixed).toContain(">Install all</button>");
	expect(mixed).not.toContain("Allow all");
	// One of the two already runs, so a single install is left to start.
	const walks = [
		{ path: app("cemu"), removing: false, seconds: 3, installing: true },
	];
	expect(pending({ ...two, walks })).not.toContain("Install all");
	// Folders alone keep their own button.
	const folders = pending(asks({ path: "/a", write: false }, { path: "/b" }));
	expect(folders).toContain(">Allow all</button>");
});

test("a source counts the folders it can't read apart from what it asks to install", () => {
	const at = (path: string, extra = {}) => ({ path, write: false, ...extra });
	const access = (...rows: object[]) =>
		({
			plugin: "rom-manager",
			grants: [],
			denied: [],
			pending: rows,
		}) as unknown as PluginAccessSnapshot;
	const cores = "/home/u/.config/retroarch/cores";
	const installs = access(
		at("/a/cemu/app", { emulator: "cemu" }),
		at("/a/dolphin/app", { emulator: "dolphin" }),
		at(cores, { core: "snes9x" }),
		at(cores, { core: "mgba" }),
	);
	expect(accessLine(installs, "ROM Manager")).toBe(
		"ROM Manager asks to install 3 emulators",
	);
	expect(
		accessLine(access(at("/roms"), ...installs.pending), "ROM Manager"),
	).toBe("ROM Manager can't read 1 of its folders");
});
