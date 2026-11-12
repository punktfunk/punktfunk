import { Link } from "@tanstack/react-router";
import Section from "@unom/ui/section";
import { Plus, SlidersHorizontal } from "lucide-react";
import { type FC, useState } from "react";
import { useGetPluginAccess } from "@/api/gen/plugin-access/plugin-access";
import { usePluginsReachable, useProfileView } from "@/api/seat";
import { SeatScope, SeatTitle } from "@/components/seat-scope";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";
import { LibraryGridSection } from "./LibraryGrid";
import { MetadataSourcesSection } from "./MetadataSources";
import { SharedNote } from "./SharedNote";
import { SourcesSection, useSourceNames } from "./Sources";
import { SharedOverridesDialog } from "./Stream";

type Tab = "games" | "sources";

/**
 * The title, which is whose library it is, **Shared overrides** and **Add**. A profile's view
 * offers neither: the titles and the shared overrides are the owner's library's.
 */
export const LibraryHeader: FC<{ onShared?: () => void }> = ({ onShared }) => {
	const view = useProfileView();
	return (
		<div className="flex flex-col gap-2">
			<div className="flex flex-wrap items-center justify-between gap-4">
				<SeatTitle title={m.library_title()} />
				{!view && (
					<div className="flex items-center gap-3">
						{onShared && (
							<Button variant="outline" onClick={onShared}>
								<SlidersHorizontal className="size-4" />
								{m.library_stream_shared()}
							</Button>
						)}
						<Button asChild>
							<Link to="/library/$gameId" params={{ gameId: "new" }}>
								<Plus className="size-4" />
								{m.library_add_button()}
							</Link>
						</Button>
					</div>
				)}
			</div>
			<SharedNote />
		</div>
	);
};

export const SectionLibrary: FC = () => (
	<SeatScope page="library">
		<Library />
	</SeatScope>
);

// Library = the games, and where they come from. Games lead: a library of thousands must not
// sit under its own settings. Adding or editing an entry happens on its own page
// (`/library/$gameId`, `/library/new`).
const Library: FC = () => {
	useLocale();
	const view = useProfileView();
	const [tab, setTab] = useState<Tab>("games");
	// Which provider (if any) the games are narrowed to.
	const [providerFilter, setProviderFilter] = useState<string | null>(null);
	const [shared, setShared] = useState(false);
	const nameOf = useSourceNames();
	const reachable = usePluginsReachable();
	// Folder and install requests wait under Sources; the tab says so while Games is open. They
	// are the owner's library's, so a profile's view has none.
	const access = useGetPluginAccess({ query: { enabled: !view } });
	const waiting = view
		? 0
		: (access.data ?? []).reduce((n, row) => n + row.pending.length, 0);

	return (
		<Section maxWidth={false}>
			<div className="flex flex-col gap-card">
				<LibraryHeader onShared={() => setShared(true)} />
				{shared && <SharedOverridesDialog onClose={() => setShared(false)} />}

				<Tabs value={tab} onValueChange={(v) => setTab(v as Tab)}>
					<TabsList>
						<TabsTrigger value="games">{m.library_tab_games()}</TabsTrigger>
						<TabsTrigger value="sources">
							{m.library_tab_sources()}
							{waiting > 0 && (
								<Badge variant="secondary" className="ml-2">
									{waiting}
								</Badge>
							)}
						</TabsTrigger>
					</TabsList>
					<TabsContent value="games" className="flex flex-col gap-card">
						<LibraryGridSection
							providerFilter={providerFilter}
							source={
								providerFilter
									? {
											label: nameOf(providerFilter) ?? providerFilter,
											onClear: () => setProviderFilter(null),
										}
									: undefined
							}
							onSources={() => setTab("sources")}
						/>
					</TabsContent>
					<TabsContent value="sources" className="flex flex-col gap-card">
						<SourcesSection
							activeFilter={providerFilter}
							onFilter={(provider) => {
								setProviderFilter(provider);
								// Narrowing is a question about the games: answer it where they are.
								if (provider) setTab("games");
							}}
							// Art & Metadata sources are plugins: only those the console reaches, and
							// only in the owner's library, whose they are.
							metadata={
								reachable && !view ? <MetadataSourcesSection /> : undefined
							}
						/>
					</TabsContent>
				</Tabs>
			</div>
		</Section>
	);
};
