import type { ReactNode } from "react";
import { Button } from "@/components/ui/button";

/**
 * One of a few values, every option visible: the console's one segmented control. The picked
 * option is filled; the rest are outlined.
 */
export function Segmented<V extends string | number | boolean>({
	value,
	options,
	onPick,
	busy,
	label,
}: {
	value: V | undefined;
	options: readonly (readonly [V, ReactNode])[];
	onPick: (value: V) => void;
	busy?: boolean;
	/** Names the group for a screen reader when no legend does. */
	label?: string;
}) {
	return (
		// biome-ignore lint/a11y/useSemanticElements: a labelled role="group" of toggle buttons; no single element fits.
		<div className="flex flex-wrap gap-2" role="group" aria-label={label}>
			{options.map(([id, text]) => (
				<Button
					key={String(id)}
					size="sm"
					variant={value === id ? "default" : "outline"}
					aria-pressed={value === id}
					disabled={busy}
					onClick={(e) => {
						// Rows that are themselves clickable must not also take this click.
						e.stopPropagation();
						onPick(id);
					}}
				>
					{text}
				</Button>
			))}
		</div>
	);
}
