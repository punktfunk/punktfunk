import { AlertTriangle, Ban, Check, Download, Search } from "lucide-react";
import { type FC, useMemo, useState } from "react";
import type { CatalogEntry } from "@/api/gen/model";
import { useGetPluginCatalog } from "@/api/gen/store/store";
import { pluginIcon } from "@/api/plugins";
import { QueryState } from "@/components/query-state";
import { Stagger } from "@/components/stagger";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { isBoolean, useLocalPref } from "@/lib/prefs";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";
import { GROUPS, type Group, groupCatalog, groupOf } from "./categories";
import { RunnerBanner } from "./Runner";
import { SourceChip, TierBadge } from "./TierBadge";

/** The same names the Library section gives its game and art sources. */
const GROUP_LABEL: Record<Group, () => string> = {
	library: m.library_sources_title,
	metadata: m.library_metadata_title,
	tools: m.store_category_tools,
	other: m.store_category_other,
};

/** Case-insensitive substring match across the fields an operator would actually search by. */
function matches(entry: CatalogEntry, needle: string): boolean {
	if (!needle) return true;
	const q = needle.toLowerCase();
	return [
		entry.title,
		entry.description,
		entry.pkg,
		entry.author,
		GROUP_LABEL[groupOf(entry)](),
	].some((f) => f.toLowerCase().includes(q));
}

/**
 * Container: the catalog, one heading per group. Owns the catalog query plus the local search,
 * group and source filters; installing is escalated to the parent, which owns the tier-appropriate
 * dialog and the resulting job — so this subsection never installs anything itself.
 */
export const BrowseTab: FC<{
	onInstall: (entry: CatalogEntry) => void;
	onInstallSpec: () => void;
}> = ({ onInstall, onInstallSpec }) => {
	const catalog = useGetPluginCatalog();
	// Sources that could not be fetched — the difference between "this host has no plugins" and
	// "the console could not find out".
	const failedSources = (catalog.data?.sources ?? []).filter(
		(src) => src.error || src.stale,
	);
	const [query, setQuery] = useState("");
	const [source, setSource] = useState<string | null>(null);
	const [group, setGroup] = useState<Group | null>(null);
	// A Linux operator should not scroll past Windows plugins to find theirs
	// (design/web-console-overhaul.md D9). The host decides what `compatible` means; this only
	// decides whether to show the rest. Off by default, remembered per browser.
	const [allPlatforms, setAllPlatforms] = useLocalPref(
		"pf-store-all-platforms",
		false,
		isBoolean,
	);

	const entries = catalog.data?.plugins ?? [];
	const sources = catalog.data?.sources ?? [];
	const forHost = useMemo(
		() => entries.filter((e) => allPlatforms || e.compatible),
		[entries, allPlatforms],
	);
	const hiddenCount = entries.length - forHost.length;
	const present = GROUPS.filter((g) => forHost.some((e) => groupOf(e) === g));
	const shown = useMemo(
		() =>
			groupCatalog(
				forHost.filter(
					(e) =>
						(source === null || e.source === source) &&
						(group === null || groupOf(e) === group) &&
						matches(e, query),
				),
			),
		[forHost, source, group, query],
	);

	return (
		<div className="flex flex-col gap-card">
			<RunnerBanner />

			<div className="flex flex-col gap-3 sm:flex-row sm:items-center">
				<div className="relative sm:max-w-xs sm:flex-1">
					<Search className="pointer-events-none absolute left-3 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" />
					<Input
						type="search"
						className="pl-9"
						aria-label={m.store_search_placeholder()}
						placeholder={m.store_search_placeholder()}
						value={query}
						onChange={(e) => setQuery(e.target.value)}
					/>
				</div>
				{/* One chip per source, so an operator can see a third-party catalog's entries alone.
				    Beside the search they take its height. */}
				{sources.length > 1 && (
					<div className="flex flex-wrap gap-2">
						<Button
							size="sm"
							className="sm:h-input-height"
							variant={source === null ? "default" : "outline"}
							aria-pressed={source === null}
							onClick={() => setSource(null)}
						>
							{m.store_filter_all()}
						</Button>
						{sources.map((s) => (
							<Button
								key={s.name}
								size="sm"
								className="sm:h-input-height"
								variant={source === s.name ? "default" : "outline"}
								aria-pressed={source === s.name}
								onClick={() => setSource(s.name)}
							>
								{s.name}
							</Button>
						))}
					</div>
				)}
				{/* Only offered when it would reveal something — an all-compatible catalog does
				    not need a filter for the empty set. */}
				{(hiddenCount > 0 || allPlatforms) && (
					<div className="flex items-center gap-2 sm:ml-auto">
						<Checkbox
							id="store-all-platforms"
							checked={allPlatforms}
							onCheckedChange={(v) => setAllPlatforms(v === true)}
						/>
						<Label
							htmlFor="store-all-platforms"
							className="text-sm font-normal text-muted-foreground"
						>
							{m.store_all_platforms()}
						</Label>
					</div>
				)}
			</div>

			{present.length > 1 && (
				<div className="flex flex-wrap gap-2">
					{[null, ...present].map((g) => (
						<Button
							key={g ?? "all"}
							size="sm"
							variant={group === g ? "default" : "outline"}
							aria-pressed={group === g}
							onClick={() => setGroup(g)}
						>
							{g === null ? m.store_category_all() : GROUP_LABEL[g]()}
						</Button>
					))}
				</div>
			)}

			<QueryState
				isLoading={catalog.isLoading}
				error={catalog.error}
				refetch={catalog.refetch}
			>
				{shown.length === 0 ? (
					<Card>
						<CardContent
							flush
							className="p-8 text-center text-sm text-muted-foreground"
						>
							{entries.length > 0
								? m.store_no_match()
								: failedSources.length > 0
									? // An all-sources-failed catalog is a SUCCESSFUL request that happens to
										// carry nothing, so "no plugins available" was the console reporting a
										// broken fetch as an empty store. Name the sources that failed.
										m.store_all_sources_failed({
											sources: failedSources.map((f) => f.name).join(", "),
										})
									: m.store_empty()}
						</CardContent>
					</Card>
				) : (
					<div className="@container">
						{/* `root` because this is a tab panel behind a query — two layers between
						    it and the page's `<Section>` that decide for themselves when to mount.
						    Each group's grid inherits its cadence through the plain `<section>`. */}
						<Stagger root className="flex flex-col gap-8">
							{shown.map(([g, list]) => (
								<section key={g} className="flex flex-col gap-3">
									<h2 className="text-base font-semibold">
										{GROUP_LABEL[g]()}{" "}
										<span className="font-normal text-muted-foreground">
											{list.length}
										</span>
									</h2>
									<Stagger className="grid grid-cols-1 gap-card @xl:grid-cols-2 @4xl:grid-cols-3">
										{list.map((entry) => (
											<StoreCard
												key={`${entry.source}/${entry.id}`}
												entry={entry}
												onInstall={() => onInstall(entry)}
											/>
										))}
									</Stagger>
								</section>
							))}
						</Stagger>
					</div>
				)}
			</QueryState>

			{/* The ONLY way to the raw-spec install. Deliberately a quiet footer link, not a button on
			    a card: an unverified install should take a decision, never a stray click. */}
			<div className="text-center">
				<button
					type="button"
					onClick={onInstallSpec}
					className="text-xs text-muted-foreground underline underline-offset-4 transition-colors hover:text-foreground"
				>
					{m.store_spec_open()}
				</button>
			</div>
		</div>
	);
};

/**
 * The catalog stores platform IDENTIFIERS (`linux | windows | macos` — see the index
 * validator); these are their display names. Proper nouns, so deliberately not routed
 * through i18n, and `macos` is spelled the way Apple spells it.
 */
const PLATFORM_LABELS: Record<string, string> = {
	linux: "Linux",
	windows: "Windows",
	macos: "macOS",
};

/**
 * `CardContent` zeroes its top padding (`pt-0`/`sm:pt-0`) because it normally sits under a
 * `CardHeader` that already supplies it — these cards have no header, so the top padding
 * has to come back explicitly, at BOTH breakpoints. `p-card` alone does not do it: `card`
 * is a custom `--spacing-*` token, which tailwind-merge does not recognise as a spacing
 * value and therefore never dedupes against `pt-0`, leaving the longhand to win.
 */
const HEADERLESS_CARD_PADDING = "p-card pt-card sm:pt-card";

/** One catalog entry. Blocked entries shout; incompatible ones grey out; neither can be installed. */
export const StoreCard: FC<{ entry: CatalogEntry; onInstall: () => void }> = ({
	entry,
	onInstall,
}) => {
	const Icon = pluginIcon(entry.icon);
	const blocked = entry.blocked != null;
	const installed = entry.installed_version != null;
	const installable = !blocked && entry.compatible;

	return (
		<Card
			className={cn(
				"flex flex-col",
				blocked && "ring-2 ring-destructive/60",
				!entry.compatible && !blocked && "opacity-60",
			)}
		>
			<CardContent
				className={cn("flex flex-1 flex-col gap-3", HEADERLESS_CARD_PADDING)}
			>
				<div className="flex items-start gap-3">
					<span className="flex size-10 shrink-0 items-center justify-center rounded-md bg-primary/15">
						<Icon className="size-5 text-foreground" />
					</span>
					<div className="min-w-0 flex-1">
						<h3 className="truncate font-medium" title={entry.title}>
							{entry.title}
						</h3>
						<p className="truncate text-xs text-muted-foreground">
							{m.store_by_author({ author: entry.author })} · v{entry.version}
						</p>
					</div>
				</div>

				<div className="flex flex-wrap items-center gap-2">
					<TierBadge tier={entry.tier} />
					{/* Attribution, never verification: an external entry names who curated it. */}
					{entry.tier === "external" && <SourceChip source={entry.source} />}
					{/* Tri-state: no probe for this platform is "unknown", never "not installed". */}
					{entry.detected === true && (
						<Badge variant="secondary">{m.library_source_detected()}</Badge>
					)}
				</div>

				<p className="line-clamp-3 text-sm text-muted-foreground">
					{entry.description}
				</p>

				<div className="flex flex-wrap gap-1.5">
					{entry.platforms.map((p) => (
						<Badge key={p} variant="secondary" className="font-normal">
							{PLATFORM_LABELS[p] ?? p}
						</Badge>
					))}
				</div>

				{blocked && (
					<p className="flex items-start gap-2 rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm font-medium text-destructive">
						<Ban className="mt-0.5 size-4 shrink-0" />
						<span>{m.store_blocked({ reason: entry.blocked ?? "" })}</span>
					</p>
				)}

				{!entry.compatible && !blocked && (
					<p className="flex items-start gap-2 text-xs text-amber-600 dark:text-amber-500">
						<AlertTriangle className="mt-px size-3.5 shrink-0" />
						<span>{entry.incompatible_reason ?? m.store_incompatible()}</span>
					</p>
				)}

				{/* Footer pinned to the bottom so cards in a row line their actions up. */}
				<div className="mt-auto flex items-center gap-3 pt-1">
					{entry.update_available ? (
						<Button size="sm" disabled={!installable} onClick={onInstall}>
							<Download className="size-4" />
							{m.store_update_to({ version: entry.version })}
						</Button>
					) : installed ? (
						<span className="inline-flex items-center gap-1.5 text-sm text-muted-foreground">
							<Check className="size-4" />
							{m.store_installed_label()}
						</span>
					) : (
						<Button size="sm" disabled={!installable} onClick={onInstall}>
							<Download className="size-4" />
							{m.store_install()}
						</Button>
					)}
					{entry.homepage && (
						<a
							href={entry.homepage}
							target="_blank"
							rel="noreferrer"
							className="ml-auto text-xs text-muted-foreground underline underline-offset-4 transition-colors hover:text-foreground"
						>
							{m.store_homepage()}
						</a>
					)}
				</div>
			</CardContent>
		</Card>
	);
};
