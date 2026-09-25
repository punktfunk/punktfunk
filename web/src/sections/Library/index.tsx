import { Link } from "@tanstack/react-router";
import Section from "@unom/ui/section";
import { Plus } from "lucide-react";
import { type FC, useState } from "react";
import { Button } from "@/components/ui/button";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";
import { LibraryGridSection } from "./LibraryGrid";
import { MetadataSourcesSection } from "./MetadataSources";
import { SourcesSection } from "./Sources";

// Library = the sources and the OVERVIEW grid. Adding or editing an entry happens on its own page
// (`/library/$gameId`, `/library/new`).
export const SectionLibrary: FC = () => {
	useLocale();
	// Which provider (if any) the grid is filtered to.
	const [providerFilter, setProviderFilter] = useState<string | null>(null);

	return (
		<Section maxWidth={false}>
			<div className="flex flex-col gap-card">
				<div className="flex items-center justify-between gap-4">
					<h1 className="text-2xl font-semibold">{m.library_title()}</h1>
					<Button asChild>
						<Link to="/library/$gameId" params={{ gameId: "new" }}>
							<Plus className="size-4" />
							{m.library_add_button()}
						</Link>
					</Button>
				</div>

				<SourcesSection
					activeFilter={providerFilter}
					onFilter={setProviderFilter}
				/>

				<MetadataSourcesSection />

				<LibraryGridSection providerFilter={providerFilter} />
			</div>
		</Section>
	);
};
