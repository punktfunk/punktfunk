import type { FC } from "react";
import { useGetHostInfo, useListCompositors } from "@/api/gen/host/host";
import { QueryState } from "@/components/query-state";
import { useLocale } from "@/lib/i18n";
import { HostSettings } from "@/sections/HostSettings";
import { AudioWiringSection } from "./AudioWiring";
import { ConflictsCard } from "./ConflictsCard";
import { HostStrip } from "./Strip";
import { UpdateCard, updateNeedsCard, useUpdate } from "./UpdateCard";

/**
 * Host (design/web-console-structure-2026-10.md §5.6): one page — a strip that says who this host
 * is, then the settings groups. A competing server is the amber line on top; the update card
 * unfolds only while there is an update to act on.
 */
export const SectionHost: FC = () => {
	useLocale();
	const host = useGetHostInfo();
	const compositors = useListCompositors();
	const update = useUpdate();
	return (
		<HostSettings
			top={
				<>
					<ConflictsCard />
					<QueryState
						isLoading={host.isLoading}
						error={host.error}
						refetch={host.refetch}
					>
						{host.data && (
							<HostStrip
								host={host.data}
								compositors={compositors.data}
								update={update}
								audio={<AudioWiringSection />}
							/>
						)}
					</QueryState>
					{updateNeedsCard(update) && <UpdateCard {...update} />}
				</>
			}
		/>
	);
};
