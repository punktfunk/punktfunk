import { type FC, useEffect, useRef, useState } from "react";
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
	// The detail card renders under the list: bring it into view, or a phone never sees it open.
	const detailRef = useRef<HTMLDivElement>(null);
	useEffect(() => {
		if (!selectedId) return;
		const still = matchMedia("(prefers-reduced-motion: reduce)").matches;
		detailRef.current?.scrollIntoView({
			behavior: still ? "auto" : "smooth",
			block: "start",
		});
	}, [selectedId]);

	return (
		<StatsView
			control={<CaptureControlSection />}
			live={<LiveSection />}
			recordings={
				<RecordingsSection selectedId={selectedId} onSelect={setSelectedId} />
			}
			detail={
				selectedId ? (
					<div ref={detailRef} className="scroll-mt-20">
						<DetailSection
							id={selectedId}
							onClose={() => setSelectedId(null)}
						/>
					</div>
				) : null
			}
		/>
	);
};
