// A seat on a person's own account: the pages behind its proxy offer nothing its host refuses,
// and its library view offers nothing that is the owner's library's.
import { expect, test } from "bun:test";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { getListLibraryScannersQueryKey } from "@/api/gen/library/library";
import type { CatalogEntry } from "@/api/gen/model";
import { getGetPluginCatalogQueryKey } from "@/api/gen/store/store";
import { type OwnAccount, type SeatPage, seatClient } from "@/api/seat";
import { DialogsProvider } from "@/components/dialogs";
import { SeatScopeView } from "@/components/seat-scope";
import { baseLocale, overwriteGetLocale } from "@/paraglide/runtime";
import { SourcesSection } from "./Library/Sources";
import { BrowseTab } from "./Store/Browse";
import { StoreHere } from "./Store/index";

overwriteGetLocale(() => baseLocale);

const entry = (title: string, tier: string): CatalogEntry =>
	({
		id: title.toLowerCase(),
		pkg: title.toLowerCase(),
		title,
		tier,
		source: tier === "verified" ? "unom" : "friends",
		author: "unom",
		version: "1.0.0",
		description: "",
		categories: ["library"],
		platforms: ["linux", "windows"],
		compatible: true,
		update_available: false,
	}) as CatalogEntry;

const STEAM = [
	{ id: "steam", label: "Steam", enabled: true, origin: "plugin" },
];
const CATALOG = {
	plugins: [entry("RomM", "verified"), entry("Theirs", "external")],
	sources: [],
	busy: false,
};

let next = 0;
/**
 * `page` as Mia's, whose seat runs on `own`, with a reviewed and an external catalog entry. On
 * Plugins her seat's host answers; the library is the owner's, seen as her.
 */
function render(page: ReactNode, on: SeatPage, own?: OwnAccount): string {
	const id = `${(next++).toString(16)}`.padStart(12, "a");
	const root = new QueryClient();
	const seat = { id, name: "Mia", ...(own ? { own } : {}) };
	const client = on === "library" ? root : seatClient(id, root);
	client.setQueryData(
		getListLibraryScannersQueryKey(on === "library" ? { as: id } : undefined),
		STEAM,
	);
	client.setQueryData(getGetPluginCatalogQueryKey(), CATALOG);
	const owner = { id: null, name: "Enrico", seat: null };
	const mia =
		on === "library"
			? { id, name: "Mia", seat: null, as: id }
			: { id, name: "Mia", seat };
	return renderToStaticMarkup(
		<QueryClientProvider client={root}>
			<DialogsProvider>
				<SeatScopeView
					page={on}
					choices={[owner, mia]}
					current={mia}
					pick={() => {}}
				>
					{page}
				</SeatScopeView>
			</DialogsProvider>
		</QueryClientProvider>,
	);
}

const UNMANAGED =
	"Mia&#x27;s desktop runs on their own account, so the console manages no plugins or emulators there.";
const sources = <SourcesSection activeFilter={null} onFilter={() => {}} />;
const browse = <BrowseTab onInstall={() => {}} onInstallSpec={() => {}} />;

test("a profile's library view switches its sources and installs nothing", () => {
	for (const own of [undefined, "linux", "windows"] as const) {
		const html = render(sources, "library", own);
		expect(html).toContain("Steam");
		expect(html).toContain("Turn one off to hide its games from Mia.");
		expect(html).not.toContain("RomM");
		expect(html).not.toContain("Theirs");
		expect(html).not.toContain(UNMANAGED);
	}
});

test("on a person's own Linux account a sentence stands in for the store", () => {
	expect(render(<StoreHere />, "plugins", "linux")).toContain(UNMANAGED);
});

test("on a person's own Windows account only Verified plugins are offered", () => {
	const store = render(browse, "plugins", "windows");
	expect(store).toContain(
		"Mia&#x27;s desktop runs on their own account, so only Verified plugins install there.",
	);
	expect(store).toContain("RomM");
	expect(store).not.toContain("Theirs");
	expect(store).not.toContain("Install from package spec");
	const box = render(browse, "plugins");
	expect(box).toContain("Theirs");
	expect(box).toContain("Install from package spec");
});
