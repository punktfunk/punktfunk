import { Link } from "@tanstack/react-router";
import Section from "@unom/ui/section";
import { Plus } from "lucide-react";
import { type FC, useState } from "react";
import { useGetPluginAccess } from "@/api/gen/plugin-access/plugin-access";
import { useSeat } from "@/api/seat";
import { SeatChip, SeatScope } from "@/components/seat-scope";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";
import { LibraryGridSection } from "./LibraryGrid";
import { MetadataSourcesSection } from "./MetadataSources";
import { SourcesSection, useSourceNames } from "./Sources";

type Tab = "games" | "sources";

/** The title, **Whose library** and **Add**. */
export const LibraryHeader: FC = () => (
	<div className="flex flex-wrap items-center justify-between gap-4">
		<h1 className="text-2xl font-semibold">{m.library_title()}</h1>
		<div className="flex items-center gap-3">
			<SeatChip />
			<Button asChild>
				<Link to="/library/$gameId" params={{ gameId: "new" }}>
					<Plus className="size-4" />
					{m.library_add_button()}
				</Link>
			</Button>
		</div>
	</div>
);

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
	const [tab, setTab] = useState<Tab>("games");
	// Which provider (if any) the games are narrowed to.
	const [providerFilter, setProviderFilter] = useState<string | null>(null);
	const nameOf = useSourceNames();
	const seat = useSeat();
	// Folder and install requests wait under Sources; the tab says so while Games is open.
	const access = useGetPluginAccess();
	const waiting = (access.data ?? []).reduce(
		(n, row) => n + row.pending.length,
		0,
	);

	return (
		<Section maxWidth={false}>
			<div className="flex flex-col gap-card">
				<LibraryHeader />

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
						/>
						{/* Art & Metadata sources are the box's plugins, and a seat has no say in them. */}
						{!seat && <MetadataSourcesSection />}
					</TabsContent>
				</Tabs>
			</div>
		</Section>
	);
};
