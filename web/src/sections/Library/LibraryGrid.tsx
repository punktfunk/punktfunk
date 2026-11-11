import {
	keepPreviousData,
	useInfiniteQuery,
	useQueryClient,
} from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { toast } from "@unom/ui/toast";
import { LayoutGrid, Rows3, Search, SquareCheck, X } from "lucide-react";
import { motion } from "motion/react";
import { type FC, useEffect, useRef, useState } from "react";
import { useDownloads } from "@/api/downloads";
import {
	getGetLibraryPageQueryKey,
	getLibraryPage,
	useDeleteCustomGame,
	useGetLibraryOverrides,
	useGetLibraryPage,
	useSetGameOverrides,
	useSetLibraryEntryHidden,
} from "@/api/gen/library/library";
import type { Download } from "@/api/gen/model/download";
import type { OperatorGameEntry } from "@/api/gen/model/operatorGameEntry";
import type { PlatformCount } from "@/api/gen/model/platformCount";
import { useDialogs } from "@/components/dialogs";
import { LauncherIcon } from "@/components/launcher-icon";
import { QueryState } from "@/components/query-state";
import { ROW, ROW_GAP, Stagger } from "@/components/stagger";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { Spinner } from "@/components/ui/spinner";
import { apiErrorMessage } from "@/lib/errors";
import { useLocale } from "@/lib/i18n";
import { useLocalPref } from "@/lib/prefs";
import type { Loadable } from "@/lib/query";
import { useDebounced } from "@/lib/use-debounced";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";
import { GameCard } from "./GameCard";
import { GameRow } from "./GameRow";
import { customId, refreshLibrary } from "./helpers";
import { DownloadsList } from "./Install";
import { setList } from "./overrides";
import { useSourceNames } from "./Sources";
import { refreshOverrides } from "./Stream";

/** Titles asked for at a time: a few rows of a wide grid, a screen and a half of the list. */
const PAGE = 60;
/** The stagger between cards of a page. A page enters in under a second, not one card a beat. */
const CARD_GAP = 0.012;
/** The `<Select>` value for no platform filter; a platform is never named this. */
const ALL = "__all__";

export type LibraryView = "grid" | "rows";
const isView = (v: unknown): v is LibraryView => v === "grid" || v === "rows";

/**
 * Container: the library OVERVIEW. The host pages the list by cursor and searches it, so a
 * library of thousands costs one page at a time; this owns the filters, the pages, per-title
 * delete and hide. A card or row opens the entry's own page.
 */
export const LibraryGridSection: FC<{
	/** Show only entries owned by this provider, or everything when null. */
	providerFilter?: string | null;
	/** The source the games are narrowed to, named, and how to widen them again. */
	source?: { label: string; onClear: () => void };
	/** Opens where sources are added: the way out of an empty library. */
	onSources?: () => void;
}> = ({ providerFilter, source, onSources }) => {
	const locale = useLocale();
	const qc = useQueryClient();
	const { confirm } = useDialogs();
	const [query, setQuery] = useState("");
	const q = useDebounced(query.trim(), 250);
	const [platform, setPlatform] = useState<string | null>(null);
	const [install, setInstall] = useState<InstallFilter | null>(null);
	const downloads = useDownloads();
	const [view, setView] = useLocalPref<LibraryView>(
		"pf-library-view",
		"grid",
		isView,
	);

	const filters = {
		role: "game",
		limit: PAGE,
		...(q ? { q } : {}),
		...(platform ? { platform } : {}),
		...(install ? { install } : {}),
		...(providerFilter ? { provider: providerFilter } : {}),
	};
	const games = useInfiniteQuery({
		queryKey: [...getGetLibraryPageQueryKey(filters), "pages"],
		queryFn: ({ pageParam, signal }) =>
			getLibraryPage(
				{ ...filters, ...(pageParam ? { cursor: pageParam } : {}) },
				{ signal },
			),
		initialPageParam: undefined as string | undefined,
		getNextPageParam: (last) => last.next_cursor ?? undefined,
		// Typing must not blank the grid between two answers.
		placeholderData: keepPreviousData,
	});
	const launchers = useGetLibraryPage({
		role: "launcher",
		limit: 200,
		...(providerFilter ? { provider: providerFilter } : {}),
	});

	const refresh = () => void refreshLibrary(qc);
	const remove = useDeleteCustomGame();

	// A refused delete has to say so. The host has real reasons to say no (a provider-owned entry
	// answers 409 with what to do instead), and an un-caught `mutateAsync` rejection reports none.
	const onDelete = async (entry: OperatorGameEntry) => {
		const ok = await confirm({
			title: m.library_delete_confirm(),
			description: m.library_delete_body(),
			confirmLabel: m.library_delete(),
			destructive: true,
		});
		if (!ok) return;
		try {
			await remove.mutateAsync({ id: customId(entry) });
		} catch (e) {
			toast.error(apiErrorMessage(e) ?? m.library_delete_failed());
			return;
		}
		refresh();
	};

	const setHidden = useSetLibraryEntryHidden();
	const nameOf = useSourceNames();

	// Same error discipline as delete: the host can refuse (it cannot persist the settings file).
	const onToggleHidden = async (entry: OperatorGameEntry) => {
		try {
			await setHidden.mutateAsync({
				id: entry.id,
				data: { hidden: entry.hidden !== true },
			});
		} catch (e) {
			toast.error(apiErrorMessage(e) ?? m.library_hide_failed());
			return;
		}
		refresh();
	};

	// Selection mode: one action, a shared override for every checked title, one call each.
	const [selecting, setSelecting] = useState(false);
	const [picked, setPicked] = useState<ReadonlySet<string>>(new Set());
	const [applying, setApplying] = useState(false);
	const overrides = useGetLibraryOverrides({ query: { enabled: selecting } });
	const setGame = useSetGameOverrides();
	const leave = () => {
		setSelecting(false);
		setPicked(new Set());
	};
	useEffect(() => {
		if (!selecting) return;
		// An Escape that closed a menu or a dialog is theirs.
		const onKey = (e: KeyboardEvent) => {
			if (e.key !== "Escape" || e.defaultPrevented) return;
			setSelecting(false);
			setPicked(new Set());
		};
		window.addEventListener("keydown", onKey);
		return () => window.removeEventListener("keydown", onKey);
	}, [selecting]);
	const onApply = async (set: string | null) => {
		setApplying(true);
		let failed = 0;
		for (const id of picked) {
			try {
				await setGame.mutateAsync({ id, data: set ? { set } : {} });
			} catch {
				failed++;
			}
		}
		await refreshOverrides(qc);
		setApplying(false);
		if (failed > 0)
			toast.error(m.library_stream_bulk_failed({ count: failed }));
		else leave();
	};

	const first = games.data?.pages[0];
	const byApp = new Map((downloads.data ?? []).map((d) => [d.app_id, d]));
	return (
		<>
			<DownloadsList />
			<LibraryGrid
				games={{
					data: games.data?.pages.flatMap((p) => p.items),
					isLoading: games.isLoading,
					error: games.error,
					refetch: () => void games.refetch(),
				}}
				launchers={launchers.data?.items ?? []}
				total={first?.total ?? 0}
				platforms={first?.platforms ?? []}
				hasMore={games.hasNextPage}
				loadingMore={games.isFetchingNextPage}
				onMore={() => void games.fetchNextPage()}
				query={query}
				onQuery={setQuery}
				platform={platform}
				onPlatform={setPlatform}
				install={install}
				onInstall={setInstall}
				notInstalled={first?.not_installed ?? 0}
				downloadOf={(id) => byApp.get(id)}
				filtered={
					q !== "" || platform !== null || install !== null || !!providerFilter
				}
				source={source}
				onSources={onSources}
				view={view}
				onView={setView}
				onDelete={onDelete}
				// The custom id whose delete is in flight (if any), so only that title's button disables.
				deletingId={remove.isPending ? (remove.variables?.id ?? null) : null}
				onToggleHidden={onToggleHidden}
				// Keyed by ENTRY id, not custom id — hiding addresses any store's entry, not just ours.
				hidingId={
					setHidden.isPending ? (setHidden.variables?.id ?? null) : null
				}
				nameOf={nameOf}
				selection={{
					active: selecting,
					ids: picked,
					onActive: (on) => (on ? setSelecting(true) : leave()),
					onToggle: (id) =>
						setPicked((prev) => {
							const next = new Set(prev);
							if (!next.delete(id)) next.add(id);
							return next;
						}),
					sets: setList(overrides.data, locale).map((s) => ({
						id: s.id,
						name: s.name ?? s.id,
					})),
					onApply: (set) => void onApply(set),
					busy: applying,
				}}
			/>
		</>
	);
};

/** On this host, or not yet: the filter appears once any title isn't installed. */
type InstallFilter = "installed" | "missing";

/** Selection mode: checkboxes on the titles, and one action for all the checked ones. */
export interface LibrarySelection {
	active: boolean;
	ids: ReadonlySet<string>;
	onActive: (active: boolean) => void;
	onToggle: (id: string) => void;
	/** The shared overrides to pick from, by name. */
	sets: { id: string; name: string }[];
	/** Every checked title takes shared override `set`, or none when null. */
	onApply: (set: string | null) => void;
	busy: boolean;
}

/** `Select` value for "Device decides" in the selection bar; a set id is 12 hex. */
const NO_SET = "device";

export interface LibraryGridProps {
	/** The titles loaded so far, every page in order. */
	games: Loadable<OperatorGameEntry[]>;
	launchers: OperatorGameEntry[];
	/** Titles matching the filters on the host, loaded or not. */
	total: number;
	platforms: PlatformCount[];
	hasMore: boolean;
	loadingMore: boolean;
	onMore: () => void;
	query: string;
	onQuery: (q: string) => void;
	platform: string | null;
	onPlatform: (p: string | null) => void;
	install?: InstallFilter | null;
	onInstall?: (f: InstallFilter | null) => void;
	/** Titles not installed under the other filters; the install filter shows when any are. */
	notInstalled?: number;
	/** A title's download, while it has one. */
	downloadOf?: (id: string) => Download | undefined;
	/** A search or filter is narrowing the list: an empty result is a miss, not a fresh host. */
	filtered: boolean;
	source?: { label: string; onClear: () => void };
	onSources?: () => void;
	view: LibraryView;
	onView: (v: LibraryView) => void;
	onDelete: (entry: OperatorGameEntry) => void;
	/** Custom id of the title whose delete is in flight, or null — only that one disables. */
	deletingId: string | null;
	onToggleHidden: (entry: OperatorGameEntry) => void;
	/** Entry id of the title whose hide/un-hide is in flight, or null. */
	hidingId: string | null;
	/** A source's display name by id, for the store and owner badges. */
	nameOf?: (id: string) => string | undefined;
	/** Selection mode for the bulk action; absent, the titles only open their pages. */
	selection?: LibrarySelection;
}

/** The checked count, the shared override chooser, and Done. Escape leaves too. */
const SelectionBar: FC<{ selection: LibrarySelection }> = ({ selection }) => {
	const { ids, sets, onApply, onActive, busy } = selection;
	return (
		<div className="sticky bottom-4 z-30 flex flex-wrap items-center gap-3 rounded-card border bg-card p-3">
			<p className="text-sm font-medium tabular-nums" aria-live="polite">
				{m.library_stream_selected({ count: ids.size })}
			</p>
			<div className="min-w-0 flex-1 sm:max-w-64">
				<Select
					value=""
					onValueChange={(v) => onApply(v === NO_SET ? null : v)}
					disabled={busy || ids.size === 0}
				>
					<SelectTrigger aria-label={m.library_stream_bulk()}>
						<SelectValue placeholder={m.library_stream_bulk()} />
					</SelectTrigger>
					<SelectContent>
						<SelectItem value={NO_SET}>{m.library_stream_device()}</SelectItem>
						{sets.map((s) => (
							<SelectItem key={s.id} value={s.id}>
								{s.name}
							</SelectItem>
						))}
					</SelectContent>
				</Select>
			</div>
			{busy && <Spinner className="size-4" />}
			<Button
				variant="outline"
				className="ml-auto"
				onClick={() => onActive(false)}
			>
				{m.common_done()}
			</Button>
		</div>
	);
};

/** A launcher, as a chip that opens its page: its mark and its name. Dimmed while hidden. */
const LauncherChip: FC<{ game: OperatorGameEntry }> = ({ game }) => (
	<motion.span variants={ROW}>
		<Link
			to="/library/$gameId"
			params={{ gameId: game.id }}
			title={game.hidden ? m.library_hidden_badge() : undefined}
			className={cn(
				"inline-flex h-8 items-center gap-2 rounded-full border bg-card px-3 text-sm font-medium transition-colors hover:bg-primary/10 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
				game.hidden && "opacity-50",
			)}
		>
			<span className="size-4 shrink-0 text-muted-foreground [&>svg]:size-full">
				<LauncherIcon icon={game.icon} />
			</span>
			{game.title}
		</Link>
	</motion.span>
);

/** Calls `onSeen` while the element is within a screen of the viewport. */
const Sentinel: FC<{ onSeen: () => void; active: boolean }> = ({
	onSeen,
	active,
}) => {
	const ref = useRef<HTMLDivElement>(null);
	const seen = useRef(onSeen);
	seen.current = onSeen;
	useEffect(() => {
		const el = ref.current;
		if (!el || !active || typeof IntersectionObserver === "undefined") return;
		const io = new IntersectionObserver(
			(entries) => {
				if (entries.some((e) => e.isIntersecting)) seen.current();
			},
			{ rootMargin: "800px 0px" },
		);
		io.observe(el);
		return () => io.disconnect();
	}, [active]);
	return <div ref={ref} aria-hidden className="h-px" />;
};

/** Three posters across a phone, up to seven across a wide desk. */
const GRID =
	"grid grid-cols-3 gap-3 @md:grid-cols-4 @md:gap-4 @2xl:grid-cols-5 @4xl:grid-cols-6 @6xl:grid-cols-7";

/** The library: a toolbar, the launchers as chips, and the titles as covers or as a list. */
export const LibraryGrid: FC<LibraryGridProps> = ({
	games,
	launchers,
	total,
	platforms,
	hasMore,
	loadingMore,
	onMore,
	query,
	onQuery,
	platform,
	onPlatform,
	install = null,
	onInstall,
	notInstalled = 0,
	downloadOf,
	filtered,
	source,
	onSources,
	view,
	onView,
	onDelete,
	deletingId,
	onToggleHidden,
	hidingId,
	nameOf,
	selection,
}) => {
	const shown = games.data ?? [];
	const selecting = selection?.active === true;
	const props = (game: OperatorGameEntry) => {
		const download = downloadOf?.(game.id);
		return {
			game,
			onDelete: () => onDelete(game),
			deleting: deletingId === customId(game),
			onToggleHidden: () => onToggleHidden(game),
			hiding: hidingId === game.id,
			nameOf,
			...(download ? { download } : {}),
			...(selection
				? {
						selecting,
						selected: selection.ids.has(game.id),
						onSelect: () => selection.onToggle(game.id),
					}
				: {}),
		};
	};
	const empty = shown.length === 0;
	return (
		<QueryState
			isLoading={games.isLoading}
			error={games.error}
			refetch={games.refetch}
		>
			{/* Mounts with the first page: its cascade runs over the toolbar, the launchers and the
			    covers together, never over a spinner. Its own clock: the page's ran during loading. */}
			<Stagger root className="flex flex-col gap-card">
				{/* Search and the view on one line at every width; the filters and the count below. */}
				<motion.div variants={ROW} className="flex items-center gap-2">
					<div className="relative min-w-0 flex-1 sm:max-w-sm">
						<Search className="pointer-events-none absolute left-3 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" />
						<Input
							type="search"
							className="pl-9"
							aria-label={m.library_search_placeholder()}
							placeholder={m.library_search_placeholder()}
							value={query}
							onChange={(e) => onQuery(e.target.value)}
						/>
					</div>
					<p
						className="hidden text-sm text-muted-foreground sm:ml-auto sm:block"
						aria-live="polite"
					>
						{shown.length < total
							? m.library_count_shown({ shown: shown.length, count: total })
							: m.library_count({ count: total })}
					</p>
					<div className="flex shrink-0 gap-1">
						{selection && (
							<Button
								variant={selecting ? "default" : "outline"}
								aria-pressed={selecting}
								onClick={() => selection.onActive(!selecting)}
							>
								<SquareCheck className="size-4" />
								<span className="sr-only sm:not-sr-only">
									{m.library_stream_select()}
								</span>
							</Button>
						)}
						<Button
							size="icon"
							variant={view === "grid" ? "default" : "outline"}
							aria-pressed={view === "grid"}
							aria-label={m.library_view_grid()}
							title={m.library_view_grid()}
							onClick={() => onView("grid")}
						>
							<LayoutGrid className="size-4" />
						</Button>
						<Button
							size="icon"
							variant={view === "rows" ? "default" : "outline"}
							aria-pressed={view === "rows"}
							aria-label={m.library_view_rows()}
							title={m.library_view_rows()}
							onClick={() => onView("rows")}
						>
							<Rows3 className="size-4" />
						</Button>
					</div>
				</motion.div>
				<motion.div
					variants={ROW}
					className="-mt-2 flex flex-wrap items-center gap-2 empty:hidden"
				>
					{(platforms.length > 1 || platform !== null) && (
						<div className="min-w-0 flex-1 sm:w-56 sm:flex-none">
							<Select
								value={platform ?? ALL}
								onValueChange={(v) => onPlatform(v === ALL ? null : v)}
							>
								<SelectTrigger aria-label={m.library_platform_label()}>
									<SelectValue />
								</SelectTrigger>
								<SelectContent>
									<SelectItem value={ALL}>
										{m.library_platform_all()}
									</SelectItem>
									{platforms.map((p) => (
										<SelectItem key={p.platform} value={p.platform}>
											{m.library_platform_option({
												platform: p.platform,
												count: p.count,
											})}
										</SelectItem>
									))}
								</SelectContent>
							</Select>
						</div>
					)}
					{onInstall && (notInstalled > 0 || install !== null) && (
						<div className="min-w-0 flex-1 sm:w-48 sm:flex-none">
							<Select
								value={install ?? ALL}
								onValueChange={(v) =>
									onInstall(v === ALL ? null : (v as InstallFilter))
								}
							>
								<SelectTrigger aria-label={m.library_install_filter_label()}>
									<SelectValue />
								</SelectTrigger>
								<SelectContent>
									<SelectItem value={ALL}>
										{m.library_install_filter_all()}
									</SelectItem>
									<SelectItem value="installed">
										{m.library_install_filter_installed()}
									</SelectItem>
									<SelectItem value="missing">
										{m.library_install_filter_missing({ count: notInstalled })}
									</SelectItem>
								</SelectContent>
							</Select>
						</div>
					)}
					{source && (
						<Button
							size="sm"
							variant="secondary"
							title={m.library_filter_clear()}
							onClick={source.onClear}
						>
							{m.library_filter_source({ source: source.label })}
							<X className="size-3.5" />
						</Button>
					)}
					<p className="basis-full text-sm text-muted-foreground sm:hidden">
						{shown.length < total
							? m.library_count_shown({ shown: shown.length, count: total })
							: m.library_count({ count: total })}
					</p>
				</motion.div>

				{/* A launcher is a door, not a game: one chip each, not a poster. */}
				{launchers.length > 0 && (
					<Stagger gap={ROW_GAP} className="flex flex-wrap items-center gap-2">
						<span className="mr-1 text-xs font-medium text-muted-foreground/70">
							{m.library_launchers_title()}
						</span>
						{launchers.map((g) => (
							<LauncherChip key={g.id} game={g} />
						))}
					</Stagger>
				)}
				{empty && launchers.length === 0 && !filtered ? (
					<Card>
						{/* `flush`, not a bare `p-8`: the default `sm:pt-0` would survive the override
						    (tailwind-merge only resolves conflicts within a variant) and eat the top
						    inset at ≥640px — see the CardContent doc comment. */}
						<CardContent
							flush
							className="p-8 text-center text-sm text-muted-foreground"
						>
							{/* A fresh host has no sources, so "no games" is the expected first-run
							    state. Point at the fix instead of leaving a bare empty grid. */}
							<p>{m.library_empty()}</p>
							<p className="mt-2">{m.library_empty_add_source()}</p>
							{onSources && (
								<Button className="mt-4" variant="outline" onClick={onSources}>
									{m.library_open_sources()}
								</Button>
							)}
						</CardContent>
					</Card>
				) : empty && filtered ? (
					<Card>
						<CardContent
							flush
							className="p-8 text-center text-sm text-muted-foreground"
						>
							{m.library_no_matches()}
						</CardContent>
					</Card>
				) : view === "rows" ? (
					<Card>
						<ul className="divide-y divide-border">
							{shown.map((g) => (
								<GameRow key={g.id} {...props(g)} />
							))}
						</ul>
					</Card>
				) : (
					<div className="@container">
						<Stagger gap={CARD_GAP} className={GRID}>
							{shown.map((g) => (
								<GameCard key={g.id} {...props(g)} />
							))}
						</Stagger>
					</div>
				)}
				{selection && selecting && <SelectionBar selection={selection} />}
				{hasMore && (
					<div className="flex flex-col items-center gap-2">
						<Sentinel onSeen={onMore} active={!loadingMore} />
						<Button variant="outline" disabled={loadingMore} onClick={onMore}>
							{loadingMore && <Spinner className="size-4" />}
							{m.library_load_more()}
						</Button>
					</div>
				)}
			</Stagger>
		</QueryState>
	);
};
