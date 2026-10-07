import {
	AlertCircle,
	Copy,
	Download,
	Pause,
	Play,
	ScrollText,
	Share2,
	Trash2,
} from "lucide-react";
import {
	type FC,
	type ReactNode,
	useEffect,
	useMemo,
	useRef,
	useState,
} from "react";
import { DocsLink } from "@/components/docs-link";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Spinner } from "@/components/ui/spinner";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";
import type { ShareMode } from "./export";
import { PLUGINS_SOURCE, type Row } from "./rows";

const LEVELS = ["DEBUG", "INFO", "WARN", "ERROR"] as const;
type MinLevel = (typeof LEVELS)[number];
const RANK: Record<string, number> = {
	TRACE: 0,
	DEBUG: 1,
	INFO: 2,
	WARN: 3,
	ERROR: 4,
};
const LEVEL_CLASS: Record<string, string> = {
	ERROR: "text-red-400",
	WARN: "text-amber-400",
	INFO: "text-sky-300",
	DEBUG: "text-muted-foreground",
	TRACE: "text-muted-foreground",
};

const SHOW = 1_000; // rendered rows (DOM bound)

/**
 * One selectable producer. Host and plugins are always offered; devices appear as their bundles
 * arrive.
 */
export interface SourceChoice {
	id: string;
	label: string;
	/** A device chip's second line — when that bundle landed. */
	hint?: string;
	selected: boolean;
	/** Fetching the bundle, or failed to. Host and plugins are never either. */
	state?: "loading" | "error";
}

/**
 * The log viewer: one pane, one timeline, every producer on it.
 *
 * The source control is **multi-select**, which is the whole point rather than a detail. The old
 * `All | Host | Plugins` strip could only ever isolate one producer, so the question that actually
 * brings someone here — "the client stalled at 12:03:47; what was the host doing?" — had no view
 * at all. Any combination is now expressible, and Host + one device is the interesting one.
 *
 * A line that does not parse still renders (see `rows.ts`), and the level/search filters treat an
 * unparsed row as level-less rather than hiding it: a filter must never be the reason a log looks
 * empty when it isn't.
 */
export const LogsCard: FC<{
	/** Every loaded row, merged and sorted. The card filters; the caller does not pre-filter. */
	rows: Row[];
	sources: SourceChoice[];
	onToggleSource: (id: string) => void;
	/** No device has ever uploaded — the hint that teaches the feature exists. */
	devicesEmpty?: boolean;
	/** Bundle housekeeping (download raw / delete), tucked under the viewer. */
	manage?: ReactNode;
	follow: boolean;
	onFollow: (follow: boolean) => void;
	onClear: () => void;
	onDownload: (shown: Row[]) => void;
	onShare: (shown: Row[]) => void;
	shareMode: ShareMode | null;
	dropped: boolean;
	/** The poll's failure, if any — without it a broken /logs is indistinguishable from a quiet host. */
	error?: unknown;
	isLoading?: boolean;
	onRetry?: () => void;
}> = ({
	rows,
	sources,
	onToggleSource,
	devicesEmpty,
	manage,
	follow,
	onFollow,
	onClear,
	onDownload,
	onShare,
	shareMode,
	dropped,
	error,
	isLoading,
	onRetry,
}) => {
	const [minLevel, setMinLevel] = useState<MinLevel>("DEBUG");
	const [search, setSearch] = useState("");
	const listRef = useRef<HTMLDivElement>(null);

	const selected = useMemo(
		() => new Set(sources.filter((s) => s.selected).map((s) => s.id)),
		[sources],
	);

	const matched = useMemo(() => {
		const min = RANK[minLevel] ?? 0;
		const q = search.trim().toLowerCase();
		return rows.filter(
			(r) =>
				selected.has(r.source) &&
				// An unparsed row has no level to rank. Ranking it as 0 would hide it behind any
				// filter above DEBUG, which is the one outcome a fail-soft parser must not produce —
				// so it is always in range and only the text search can exclude it.
				(r.level === "" || (RANK[r.level] ?? 0) >= min) &&
				(q === "" ||
					r.msg.toLowerCase().includes(q) ||
					r.target.toLowerCase().includes(q) ||
					(r.device?.toLowerCase().includes(q) ?? false)),
		);
	}, [rows, selected, minLevel, search]);

	const visible = useMemo(() => matched.slice(-SHOW), [matched]);
	const shareLabel = shareMode === "share" ? m.logs_share() : m.logs_copy();
	const nothingSelected = selected.size === 0;
	// "No plugin output" has a specific, actionable cause that the generic "adjust the filter" line
	// actively misdirects from: the runner is a separate service and is opt-in on Linux, so the
	// usual reason for an empty Plugins view is that it simply isn't running. Only worth saying
	// when plugins are the ONLY thing being looked at — otherwise the emptiness is not about them.
	const onlyPlugins = selected.size === 1 && selected.has(PLUGINS_SOURCE);

	// Keep the tail in view while following.
	//
	// Keyed on the newest RENDERED key, not on `visible.length`: `visible` is `matched.slice(-SHOW)`,
	// so once the filter matches SHOW rows its length is pinned at SHOW forever. The effect then
	// stopped re-running and follow-mode quietly stopped following — exactly when the log is busy
	// enough to need it. The newest key keeps changing for as long as lines arrive.
	const newestVisible = visible.at(-1)?.key ?? "";
	// NOTE: biome flags `newestVisible` as an unnecessary dependency (it is not read in the body) and
	// offers to remove it. Do NOT take that fix — it is a TRIGGER, the signal that new lines arrived.
	// Removing it reinstates the bug this replaced: the effect stops re-running and follow-mode
	// quietly stops following. The same warning was here before, on `visible.length`.
	// biome-ignore lint/correctness/useExhaustiveDependencies: newestVisible is a deliberate trigger, not a read — see the note above; removing it stops follow-mode from following.
	useEffect(() => {
		if (!follow) return;
		const el = listRef.current;
		if (el) el.scrollTop = el.scrollHeight;
	}, [follow, newestVisible]);

	return (
		<Card>
			{/* No CardHeader here, and that no longer needs saying: CardContent keeps its top inset
			    unless something precedes it. This card used to restore it by hand at both
			    breakpoints. */}
			<CardContent className="flex flex-col gap-3">
				{/* The page heading says "Troubleshooting" now, so this card names itself — otherwise
				    the log stream is the only section on the page with no label. */}
				<CardTitle>
					<h2 className="flex items-center gap-2">
						<ScrollText className="size-4" />
						{m.logs_title()}
					</h2>
				</CardTitle>

				<div className="flex flex-wrap items-center gap-2">
					<span className="text-xs text-muted-foreground">
						{m.logs_sources_label()}
					</span>
					{sources.map((s) => (
						<Button
							key={s.id}
							size="sm"
							variant={s.selected ? "secondary" : "outline"}
							aria-pressed={s.selected}
							disabled={s.state === "loading"}
							onClick={() => onToggleSource(s.id)}
						>
							{s.state === "loading" && <Spinner className="mr-1 size-3.5" />}
							{s.state === "error" && (
								<AlertCircle className="mr-1 size-3.5 text-destructive" />
							)}
							{s.label}
							{s.hint && (
								<span className="ml-1.5 text-xs text-muted-foreground">
									{s.hint}
								</span>
							)}
						</Button>
					))}
					{/* The bundle list used to vanish entirely when empty, which meant the one place
					    that could teach "your devices can send their logs here" showed nothing to
					    anyone who had never already used it. One line is not the noise a whole empty
					    card was. */}
					{devicesEmpty && (
						<span className="text-xs text-muted-foreground">
							{m.logs_devices_empty()}
						</span>
					)}
				</div>

				<div className="flex flex-wrap items-center gap-2">
					<div className="flex items-center gap-1">
						{LEVELS.map((l) => (
							<Button
								key={l}
								size="sm"
								variant={minLevel === l ? "secondary" : "ghost"}
								onClick={() => setMinLevel(l)}
							>
								{l}
							</Button>
						))}
					</div>
					<Input
						value={search}
						onChange={(e) => setSearch(e.target.value)}
						placeholder={m.logs_search()}
						className="max-w-xs"
					/>
					<div className="ml-auto flex items-center gap-2">
						{dropped && <Badge variant="secondary">{m.logs_dropped()}</Badge>}
						<Button
							size="icon"
							variant="ghost"
							disabled={matched.length === 0}
							title={m.logs_download()}
							aria-label={m.logs_download()}
							onClick={() => onDownload(matched)}
						>
							<Download className="size-4" />
						</Button>
						{shareMode && (
							<Button
								size="icon"
								variant="ghost"
								disabled={matched.length === 0}
								title={shareLabel}
								aria-label={shareLabel}
								onClick={() => onShare(matched)}
							>
								{shareMode === "share" ? (
									<Share2 className="size-4" />
								) : (
									<Copy className="size-4" />
								)}
							</Button>
						)}
						<Button
							size="sm"
							variant={follow ? "secondary" : "outline"}
							onClick={() => onFollow(!follow)}
						>
							{follow ? (
								<Pause className="mr-1 size-3.5" />
							) : (
								<Play className="mr-1 size-3.5" />
							)}
							{follow ? m.logs_pause() : m.logs_follow()}
						</Button>
						<Button size="sm" variant="ghost" onClick={onClear}>
							<Trash2 className="mr-1 size-3.5" />
							{m.logs_clear()}
						</Button>
					</div>
				</div>

				{/* A failing poll while lines are already on screen keeps them there — during a host
				    restart the last lines before it went away are the interesting ones — but says so,
				    instead of letting a frozen view read as a quiet host. */}
				{error != null && rows.length > 0 && (
					<p
						role="status"
						className="rounded-md border border-destructive/40 bg-destructive/5 px-3 py-2 text-sm text-destructive"
					>
						{m.logs_stalled()}
					</p>
				)}

				<div
					ref={listRef}
					className="max-h-[65vh] overflow-auto rounded-md border bg-card/40 p-2 font-mono text-xs leading-5"
				>
					{visible.length === 0 ? (
						// An empty list has four quite different causes and used to render one sentence
						// for all of them: the host is quiet, the request failed, it hasn't answered yet,
						// or every source is switched off.
						<div className="p-2">
							{error ? (
								<div className="space-y-2 font-sans">
									<p className="text-destructive">{m.common_error()}</p>
									{onRetry && (
										<Button size="sm" variant="outline" onClick={onRetry}>
											{m.common_retry()}
										</Button>
									)}
								</div>
							) : (
								<p className="text-muted-foreground">
									{isLoading ? (
										m.common_loading()
									) : nothingSelected ? (
										m.logs_sources_none()
									) : onlyPlugins ? (
										// A silent runner says why in its own log; the docs name it.
										<>
											{m.logs_empty_plugins()}{" "}
											<DocsLink path="plugins#troubleshooting" />
										</>
									) : (
										m.logs_empty()
									)}
								</p>
							)}
						</div>
					) : (
						visible.map((r) => (
							<div key={r.key} className="whitespace-pre-wrap break-words">
								<span className="text-muted-foreground">{fmtTime(r.ts)} </span>
								<span
									className={cn(
										"font-medium",
										LEVEL_CLASS[r.level] ?? "text-muted-foreground",
									)}
								>
									{r.level.padEnd(5)}{" "}
								</span>
								{/* Only device rows are tagged. Absence of a tag reads as "this host",
								    and stamping every host line would double the noise in the common
								    case where no bundle is loaded at all. */}
								{/* Theme-aware, unlike LEVEL_CLASS above: one fixed mid shade is legible on
								    exactly one of the two palettes, and this tag is the thing that has
								    to stay readable for a merged view to be worth having. Checked in
								    both themes, not inferred. */}
								{r.device && (
									<span className="text-violet-600 dark:text-violet-400">
										[{r.device}]{" "}
									</span>
								)}
								<span className="text-muted-foreground">{r.target} </span>
								<span>{r.msg}</span>
							</div>
						))
					)}
				</div>

				{manage}
			</CardContent>
		</Card>
	);
};

const fmtTime = (ts: number): string => {
	const d = new Date(ts);
	const p = (n: number, w = 2) => String(n).padStart(w, "0");
	return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 3)}`;
};
