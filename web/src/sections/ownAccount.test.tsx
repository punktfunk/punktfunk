// A seat on a person's own account: the pages behind its proxy offer nothing its host refuses.
import { expect, test } from "bun:test";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { getListLibraryScannersQueryKey } from "@/api/gen/library/library";
import type { CatalogEntry } from "@/api/gen/model";
import { getGetPluginCatalogQueryKey } from "@/api/gen/store/store";
import { type OwnAccount, seatClient } from "@/api/seat";
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

let next = 0;
/** `page` as seat Mia's, whose host runs on `own`, with a reviewed and an external catalog entry. */
function render(page: ReactNode, own?: OwnAccount): string {
	const id = `${(next++).toString(16)}`.padStart(32, "a");
	const root = new QueryClient();
	const seat = seatClient(id, root);
	seat.setQueryData(getListLibraryScannersQueryKey(), [
		{ id: "steam", label: "Steam", enabled: true, origin: "plugin" },
	]);
	seat.setQueryData(getGetPluginCatalogQueryKey(), {
		plugins: [entry("RomM", "verified"), entry("Theirs", "external")],
		sources: [],
		busy: false,
	});
	const choice = { id, name: "Mia", ...(own ? { own } : {}) };
	return renderToStaticMarkup(
		<QueryClientProvider client={root}>
			<DialogsProvider>
				<SeatScopeView
					page="library"
					seat={choice}
					seats={[choice]}
					ownerName="Enrico"
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

test("a seat of its own offers every source plugin", () => {
	const html = render(sources);
	expect(html).toContain("RomM");
	expect(html).toContain("Theirs");
	expect(html).not.toContain(UNMANAGED);
});

test("on a person's own Linux account a sentence stands in for plugins and emulators", () => {
	const html = render(sources, "linux");
	expect(html).toContain(UNMANAGED);
	expect(html).not.toContain("RomM");
	expect(html).not.toContain("Theirs");
	expect(render(<StoreHere />, "linux")).toContain(UNMANAGED);
});

test("on a person's own Windows account only Verified plugins are offered", () => {
	const html = render(sources, "windows");
	expect(html).toContain("RomM");
	expect(html).not.toContain("Theirs");
	const store = render(browse, "windows");
	expect(store).toContain(
		"Mia&#x27;s desktop runs on their own account, so only Verified plugins install there.",
	);
	expect(store).toContain("RomM");
	expect(store).not.toContain("Theirs");
	expect(store).not.toContain("Install from package spec");
	const box = render(browse);
	expect(box).toContain("Theirs");
	expect(box).toContain("Install from package spec");
});
