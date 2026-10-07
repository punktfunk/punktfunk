import { Download } from "lucide-react";
import { type FC, type ReactNode, useState } from "react";
import type { CatalogEntry } from "@/api/gen/model";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { m } from "@/paraglide/messages";
import { InstallDialog } from "@/sections/Store/InstallDialogs";

/**
 * Catalogued plugins a source section can add, as buttons. Installing confirms through the
 * Store's own dialog: one installer, one trust decision, the same warning for an entry from an
 * operator-added catalog.
 */
export const AddSourceRail: FC<{
	entries: CatalogEntry[];
	busy: boolean;
	onInstall: (entry: CatalogEntry) => void;
}> = ({ entries, busy, onInstall }) => {
	const [confirming, setConfirming] = useState<CatalogEntry | null>(null);
	if (entries.length === 0) return null;
	return (
		<div className="space-y-2 border-t pt-4">
			<p className="text-sm font-medium">{m.library_add_source()}</p>
			<div className="flex flex-wrap gap-2">
				{entries.map((entry) => (
					<Button
						key={entry.pkg}
						size="sm"
						variant="outline"
						disabled={busy}
						title={entry.description}
						onClick={() => setConfirming(entry)}
					>
						<Download className="size-4" />
						{entry.title}
						{/* Tri-state: no probe for this platform is "unknown", never "not installed". */}
						{entry.detected === true && (
							<Badge variant="secondary">{m.library_source_detected()}</Badge>
						)}
					</Button>
				))}
			</div>
			<InstallDialog
				entry={confirming}
				onCancel={() => setConfirming(null)}
				isPending={busy}
				onConfirm={(entry) => {
					setConfirming(null);
					onInstall(entry);
				}}
			/>
		</div>
	);
};

/** One section of the Sources tab: a heading, then its rows. */
export const SourceGroup: FC<{
	icon: ReactNode;
	title: string;
	description?: string;
	children: ReactNode;
}> = ({ icon, title, description, children }) => (
	<section className="space-y-3">
		<div className="space-y-1">
			<h2 className="flex items-center gap-2 font-semibold">
				{icon}
				{title}
			</h2>
			{description && (
				<p className="text-sm text-muted-foreground">{description}</p>
			)}
		</div>
		{children}
	</section>
);
