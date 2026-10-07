import { type FC, useState } from "react";
import { useLocale } from "@/lib/i18n";
import { CaptureControlSection } from "./CaptureControl";
import { DetailSection } from "./Detail";
import { LiveSection } from "./LiveCard";
import { RecordingsSection } from "./Recordings";
import { StatsView } from "./view";

// Performance = four independent, self-contained cards (control · live · recordings · detail), each
// owning its own queries + mutations in its own file. This container holds only the shared UI state
// — which recording is selected — that links the recordings table to the detail card. The layout
// lives in StatsView so the live page and the Storybook story arrange the cards identically.
export const SectionStats: FC = () => {
	useLocale();
	const [selectedId, setSelectedId] = useState<string | null>(null);

	return (
		<StatsView
			control={<CaptureControlSection />}
			live={<LiveSection />}
			recordings={
				<RecordingsSection selectedId={selectedId} onSelect={setSelectedId} />
			}
			detail={
				selectedId ? (
					<DetailSection id={selectedId} onClose={() => setSelectedId(null)} />
				) : null
			}
		/>
	);
};
