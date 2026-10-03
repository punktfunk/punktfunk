import {
	keepPreviousData,
	useInfiniteQuery,
	useQueryClient,
} from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { LayoutGrid, Rows3, Search, X } from "lucide-react";
import { type FC, useEffect, useRef, useState } from "react";
import { useDownloads } from "@/api/downloads";
import {
	getGetLibraryPageQueryKey,
	getLibraryPage,
	useDeleteCustomGame,
	useGetLibraryPage,
	useSetLibraryEntryHidden,
} from "@/api/gen/library/library";
import type { Download } from "@/api/gen/model/download";
import type { OperatorGameEntry } from "@/api/gen/model/operatorGameEntry";
import type { PlatformCount } from "@/api/gen/model/platformCount";
import { useDialogs } from "@/components/dialogs";
import { QueryState } from "@/components/query-state";
import { Stagger } from "@/components/stagger";
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
import { useLocalPref } from "@/lib/prefs";
import type { Loadable } from "@/lib/query";
import { useDebounced } from "@/lib/use-debounced";
import { m } from "@/paraglide/messages";
import { GameCard } from "./GameCard";
import { GameRow } from "./GameRow";
import { customId, refreshLibrary } from "./helpers";
import { DownloadsList } from "./Install";
import { useSourceNames } from "./Sources";

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
			/>
		</>
	);
};

/** On this host, or not yet: the filter appears once any title isn't installed. */
type InstallFilter = "installed" | "missing";

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
}

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

const GRID =
	"grid grid-cols-1 gap-card @sm:grid-cols-2 @md:grid-cols-2 @lg:grid-cols-3 @2xl:grid-cols-4 @4xl:grid-cols-5 @6xl:grid-cols-6";

/** The library: a toolbar, the launcher rail, and the titles as covers or as a list. */
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
}) => {
	const shown = games.data ?? [];
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
		};
	};
	const empty = !games.isLoading && shown.length === 0;
	return (
		<div className="flex flex-col gap-card">
			<div className="flex flex-col gap-3 sm:flex-row sm:items-center">
				<div className="relative sm:max-w-xs sm:flex-1">
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
				{(platforms.length > 1 || platform !== null) && (
					<div className="sm:w-56">
						<Select
							value={platform ?? ALL}
							onValueChange={(v) => onPlatform(v === ALL ? null : v)}
						>
							<SelectTrigger aria-label={m.library_platform_label()}>
								<SelectValue />
							</SelectTrigger>
							<SelectContent>
								<SelectItem value={ALL}>{m.library_platform_all()}</SelectItem>
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
					<div className="sm:w-48">
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
				<p
					className="text-sm text-muted-foreground sm:ml-auto"
					aria-live="polite"
				>
					{shown.length < total
						? m.library_count_shown({ shown: shown.length, count: total })
						: m.library_count({ count: total })}
				</p>
				<div className="flex gap-1">
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
			</div>

			<QueryState
				isLoading={games.isLoading}
				error={games.error}
				refetch={games.refetch}
			>
				{launchers.length > 0 && (
					<div className="@container">
						<p className="pb-2 text-xs font-medium uppercase tracking-wide text-muted-foreground/70">
							{m.library_launchers_title()}
						</p>
						<Stagger className={GRID}>
							{launchers.map((g) => (
								<GameCard key={g.id} {...props(g)} />
							))}
						</Stagger>
					</div>
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
				{hasMore && (
					<div className="flex flex-col items-center gap-2">
						<Sentinel onSeen={onMore} active={!loadingMore} />
						<Button variant="outline" disabled={loadingMore} onClick={onMore}>
							{loadingMore && <Spinner className="size-4" />}
							{m.library_load_more()}
						</Button>
					</div>
				)}
			</QueryState>
		</div>
	);
};
