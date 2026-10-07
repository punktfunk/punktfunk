import type { FC, ReactNode } from "react";
import { fmtDateTime } from "@/lib/format";
import { m } from "@/paraglide/messages";

/** Locale-aware (see lib/format.ts) — a bare `toLocaleString` follows the BROWSER, not the app. */
export function fmtTimestamp(unixMs: number): string {
	return fmtDateTime(unixMs);
}

export function kindLabel(kind: string): string {
	if (kind === "gamestream") return m.stats_kind_gamestream();
	if (kind === "native") return m.stats_kind_native();
	if (kind === "web") return m.stats_kind_web();
	return kind;
}

export const Stat: FC<{ label: string; value: string }> = ({
	label,
	value,
}) => (
	<div className="flex flex-col">
		<dt className="text-xs text-muted-foreground">{label}</dt>
		<dd className="font-medium">{value}</dd>
	</div>
);

export const ChartBlock: FC<{
	title: string;
	desc?: string;
	children: ReactNode;
}> = ({ title, desc, children }) => (
	<div className="space-y-2">
		<div>
			<h3 className="text-sm font-medium">{title}</h3>
			{desc && <p className="text-xs text-muted-foreground">{desc}</p>}
		</div>
		{children}
	</div>
);
