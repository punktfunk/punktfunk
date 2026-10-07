import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { Boxes, Download, PackagePlus } from "lucide-react";
import { type FC, type ReactNode, useMemo, useState } from "react";
import {
	getListLibraryScannersQueryKey,
	useDeleteProviderEntries,
	useListLibraryScanners,
	useSetLibraryScanner,
} from "@/api/gen/library/library";
import type { CatalogEntry } from "@/api/gen/model";
import type { PluginAccessSnapshot } from "@/api/gen/model/pluginAccessSnapshot";
import type { ScannerInfo } from "@/api/gen/model/scannerInfo";
import { useGetPluginCatalog } from "@/api/gen/store/store";
import { usePlugins } from "@/api/plugins";
import { useSeat } from "@/api/seat";
import { useInstallPlugin } from "@/api/store";
import { useDialogs } from "@/components/dialogs";
import { ROW_GAP, Stagger } from "@/components/stagger";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
import { MenuItem, MenuSeparator, RowMenu } from "@/components/ui/menu";
import { apiErrorMessage } from "@/lib/errors";
import { m } from "@/paraglide/messages";
import { EmulatorsCard } from "@/sections/Library/Emulators";
import { PendingAccess, usePluginAccess } from "@/sections/PluginAccess";
import { AddSourceRail, SourceGroup, SourceItem } from "./AddSource";
import { refreshLibrary } from "./helpers";
import { SourceSettingsDialog } from "./SourceSettings";

/**
 * The Sources tab: one card of sections — Game sources, Emulators, Art & metadata — with the
 * migration banner above it. Game sources: enablement, liveness, counts, actions, and pending
 * folder access in one list, because this is where missing games are visible. N-1 hosts may
 * still report built-in scanners, so migration stays until those hosts age out.
 */
export const SourcesSection: FC<{
	/** The provider currently filtered to in the grid, or null for "everything". */
	activeFilter: string | null;
	onFilter: (provider: string | null) => void;
	/** Art & metadata: the box's, absent on a seat. */
	metadata?: ReactNode;
}> = ({ activeFilter, onFilter, metadata }) => {
	const qc = useQueryClient();
	const { confirm } = useDialogs();
	const scanners = useListLibraryScanners();
	const toggle = useSetLibraryScanner();
	const purge = useDeleteProviderEntries();
	const plugins = usePlugins();
	const catalog = useGetPluginCatalog();
	const install = useInstallPlugin();
	const access = usePluginAccess();
	const [settingsFor, setSettingsFor] = useState<ScannerInfo | null>(null);
	const nameOf = useSourceNames();

	const onToggle = async (source: ScannerInfo) => {
		try {
			// The PUT answers with the full updated list — seed the query cache with it directly,
			// then refetch the library so the grid reflects the new source set.
			const list = await toggle.mutateAsync({
				id: source.id,
				data: { enabled: !source.enabled },
			});
			qc.setQueryData(getListLibraryScannersQueryKey(), list);
			await refreshLibrary(qc);
		} catch {
			toast.error(m.library_sources_failed());
		}
	};

	const onPurge = async (source: ScannerInfo) => {
		const provider = source.provider ?? source.id;
		const count = source.entries ?? 0;
		const ok = await confirm({
			title: m.library_provider_purge_confirm({
				provider: source.label,
				count,
			}),
			description: m.library_provider_purge_body(),
			confirmLabel: m.common_remove(),
			destructive: true,
		});
		if (!ok) return;
		try {
			await purge.mutateAsync({ provider });
			refreshLibrary(qc);
			qc.invalidateQueries({ queryKey: getListLibraryScannersQueryKey() });
			if (activeFilter === provider) onFilter(null);
			toast.success(m.library_provider_purged({ provider: source.label }));
		} catch (e) {
			toast.error(apiErrorMessage(e) ?? m.library_provider_purge_failed());
		}
	};

	const onInstall = async (entry: CatalogEntry) => {
		try {
			// Install by (source, id) — the catalogued, integrity-pinned path. The raw-spec form is
			// for unverified installs and must never be reachable from a one-click rail.
			await install.mutateAsync({ source: entry.source, id: entry.id });
			toast.success(m.library_source_installing({ title: entry.title }));
		} catch (e) {
			toast.error(apiErrorMessage(e) ?? m.library_source_install_failed());
		}
	};

	// This is a secondary control: when the API is down the grid's own QueryState already tells the
	// story, so render nothing rather than a second error banner.
	if (!scanners.data) return null;
	// The host names only the stores that used to be built in; any other id it shows as itself.
	const sources = scanners.data.map((s) => ({
		...s,
		label: nameOf(s.id) ?? s.label,
	}));

	// Catalog rows that are library sources and not already installed — the "Add a source" rail.
	const installedPkgs = new Set(
		(catalog.data?.plugins ?? [])
			.filter((p) => p.installed_version)
			.map((p) => p.pkg),
	);
	// Compatible only: this rail is a row of Install buttons, and one for a scanner that cannot
	// run on this OS is a control that does nothing (design/web-console-overhaul.md §2.1). The
	// full catalog, incompatible entries included, is a checkbox away on the Store page. A launcher
	// found on this host leads.
	const available = (catalog.data?.plugins ?? [])
		.filter(
			(p) =>
				p.categories?.includes("library") &&
				!installedPkgs.has(p.pkg) &&
				p.compatible,
		)
		.sort((a, b) => Number(b.detected === true) - Number(a.detected === true));
	// Every live registration, NOT just the `library`-category ones. A plugin's nav categorisation
	// cannot decide whether its liveness badge is honest: a library plugin that registers without
	// `category` is live, and filtering on it here badges a running plugin "Stopped".
	const running = new Set((plugins.data ?? []).map((p) => p.id));

	// The bridge-release nudge (design D9): a built-in scanner still doing the work, with its
	// replacement plugin sitting uninstalled in the catalog. One click per scanner, and NEVER a
	// silent auto-install — installing code stays an explicit operator act.
	//
	// Against a v0.28.0+ host this is always empty (no source reports `builtin` any more) and the
	// banner never renders. It stays for the N-1 host this console may be driving, where it is still
	// the migration path.
	const migratable = sources
		.filter((s) => s.origin === "builtin" && s.enabled)
		.map((s) => ({
			source: s,
			entry: available.find((p) => p.id === s.id),
		}))
		.filter(
			(r): r is { source: ScannerInfo; entry: CatalogEntry } => !!r.entry,
		);

	return (
		<>
			{migratable.length > 0 && (
				<MigrationBanner
					rows={migratable}
					busy={catalog.data?.busy === true || install.isPending}
					onInstall={onInstall}
				/>
			)}
			<Card>
				<CardContent className="space-y-8">
					<SourcesCard
						sources={sources}
						available={available}
						running={running}
						busyId={toggle.isPending ? (toggle.variables?.id ?? null) : null}
						installBusy={catalog.data?.busy === true || install.isPending}
						activeFilter={activeFilter}
						onToggle={onToggle}
						onFilter={onFilter}
						onSettings={setSettingsFor}
						onPurge={onPurge}
						onInstall={onInstall}
						access={access.access.data}
						accessBusy={access.busy}
						onAccessDecision={access.onDecide}
					/>
					<EmulatorsCard />
					{metadata}
				</CardContent>
			</Card>
			{settingsFor && (
				<SourceSettingsDialog
					source={settingsFor}
					onClose={() => setSettingsFor(null)}
				/>
			)}
		</>
	);
};

/**
 * "Game sources are moving to plugins" — shown only while a built-in scanner is still doing a job a
 * catalogued plugin could take over.
 *
 * One button per scanner rather than a single "migrate everything": installing a plugin is an
 * explicit operator act under the store's consent model, and per-scanner is also what makes it safe
 * to repeat — the claim suppresses the built-in idempotently (design D2), so a half-finished
 * migration is a valid state rather than a mess.
 */
export const MigrationBanner: FC<{
	rows: ReadonlyArray<{ source: ScannerInfo; entry: CatalogEntry }>;
	busy: boolean;
	onInstall: (entry: CatalogEntry) => void;
}> = ({ rows, busy, onInstall }) => (
	<Card>
		<CardHeader className="pb-3">
			<CardTitle className="flex items-center gap-2">
				<PackagePlus className="size-4" />
				{m.library_migrate_title()}
			</CardTitle>
		</CardHeader>
		<CardContent className="space-y-3">
			<p className="max-w-prose text-sm text-muted-foreground">
				{m.library_migrate_help()}
			</p>
			<div className="flex flex-wrap gap-2">
				{rows.map(({ source, entry }) => (
					<Button
						key={source.id}
						size="sm"
						variant="outline"
						disabled={busy}
						onClick={() => onInstall(entry)}
					>
						<Download className="size-4" />
						{m.library_migrate_install({ source: source.label })}
					</Button>
				))}
			</div>
		</CardContent>
	</Card>
);

/** Presentational source list, including deterministic pending-access states for Storybook. A
 * section of the Sources card, not a card of its own. */
export const SourcesCard: FC<{
	sources: ScannerInfo[];
	/** Catalog rows offering a library source that isn't installed yet. */
	available: CatalogEntry[];
	/** Ids of every plugin whose lease is currently live, whatever its category. */
	running: Set<string>;
	/** Source id whose toggle is in flight, or null — only that row disables. */
	busyId: string | null;
	installBusy: boolean;
	activeFilter: string | null;
	onToggle: (source: ScannerInfo) => void;
	onFilter: (provider: string | null) => void;
	onSettings: (source: ScannerInfo) => void;
	onPurge: (source: ScannerInfo) => void;
	onInstall: (entry: CatalogEntry) => void;
	access?: PluginAccessSnapshot[];
	accessBusy?: boolean;
	accessInitiallyOpen?: boolean;
	onAccessDecision?: (
		plugin: string,
		paths: string[],
		decision: "allow" | "deny" | "forget",
	) => void;
}> = ({
	sources,
	available,
	running,
	busyId,
	installBusy,
	activeFilter,
	onToggle,
	onFilter,
	onSettings,
	onPurge,
	onInstall,
	access = [],
	accessBusy = false,
	accessInitiallyOpen = false,
	onAccessDecision = () => {},
}) => (
	<SourceGroup
		icon={<Boxes className="size-4" />}
		title={m.library_sources_title()}
	>
		<div className="space-y-4">
			<Stagger gap={ROW_GAP} className="flex flex-col divide-y">
				{sources.map((source) => (
					<SourceRow
						key={source.id}
						source={source}
						running={running.has(source.provider ?? source.id)}
						access={access.find(
							(row) => row.plugin === (source.provider ?? source.id),
						)}
						accessBusy={accessBusy}
						accessInitiallyOpen={accessInitiallyOpen}
						onAccessDecision={onAccessDecision}
						busy={busyId === source.id}
						filtered={
							activeFilter !== null &&
							activeFilter === (source.provider ?? source.id)
						}
						onToggle={() => onToggle(source)}
						onFilter={() => {
							const p = source.provider ?? source.id;
							onFilter(activeFilter === p ? null : p);
						}}
						onSettings={() => onSettings(source)}
						onPurge={() => onPurge(source)}
					/>
				))}
			</Stagger>
			<p className="max-w-prose text-xs text-muted-foreground">
				{m.library_sources_help()}
			</p>

			<AddSourceRail
				entries={available}
				busy={installBusy}
				onInstall={onInstall}
			/>
		</div>
	</SourceGroup>
);

/**
 * One source: on/off, its name over what it is doing, the rest in ⋯. The folder requests that
 * explain missing games open below it.
 */
const SourceRow: FC<{
	source: ScannerInfo;
	/** The plugin backing this source is currently registered (its lease is live). */
	running: boolean;
	access?: PluginAccessSnapshot;
	accessBusy: boolean;
	accessInitiallyOpen: boolean;
	onAccessDecision: (
		plugin: string,
		paths: string[],
		decision: "allow" | "deny" | "forget",
	) => void;
	busy: boolean;
	filtered: boolean;
	onToggle: () => void;
	onFilter: () => void;
	onSettings: () => void;
	onPurge: () => void;
}> = ({
	source,
	running,
	access,
	accessBusy,
	accessInitiallyOpen,
	onAccessDecision,
	busy,
	filtered,
	onToggle,
	onFilter,
	onSettings,
	onPurge,
}) => {
	const isPlugin = source.origin === "plugin";
	const seat = useSeat();
	const [accessOpen, setAccessOpen] = useState(accessInitiallyOpen);
	const meta = [
		isPlugin &&
			(running ? m.library_source_running() : m.library_source_stopped()),
		source.entries != null &&
			m.library_provider_count({ count: source.entries }),
	]
		.filter(Boolean)
		.join(" · ");
	return (
		<SourceItem
			lead={
				<Checkbox
					checked={source.enabled}
					disabled={busy}
					aria-label={source.label}
					onCheckedChange={onToggle}
				/>
			}
			title={source.label}
			meta={meta || undefined}
			actions={
				isPlugin && (
					<RowMenu label={m.common_more_actions()}>
						<MenuItem onSelect={onFilter}>
							{filtered
								? m.library_provider_show_all()
								: m.library_provider_filter()}
						</MenuItem>
						{/* The form is the box's plugin; a seat has none to open. */}
						{!seat && (
							<MenuItem onSelect={onSettings}>
								{m.library_source_settings()}
							</MenuItem>
						)}
						<MenuSeparator />
						<MenuItem destructive onSelect={onPurge}>
							{m.library_provider_purge()}
						</MenuItem>
					</RowMenu>
				)
			}
		>
			{access && access.pending.length > 0 && (
				<div className="mt-2 pl-7">
					<button
						type="button"
						className="text-left text-sm font-medium text-amber-600 hover:underline dark:text-amber-500"
						onClick={() => setAccessOpen((open) => !open)}
					>
						{m.library_source_access_pending({
							title: source.label,
							count: access.pending.length,
						})}
					</button>
					{accessOpen && (
						<PendingAccess
							access={access}
							busy={accessBusy}
							onDecide={(paths, decision) =>
								onAccessDecision(access.plugin, paths, decision)
							}
						/>
					)}
				</div>
			)}
		</SourceItem>
	);
};

/**
 * A source's display name by id — the one id a source, its store claim and its provider share. The
 * plugin's own title wins (the kit's contract), then the host's label. A name that is only the id
 * again counts as none, so each caller keeps its own fallback.
 */
export const useSourceNames = (): ((id: string) => string | undefined) => {
	const scanners = useListLibraryScanners();
	const plugins = usePlugins();
	return useMemo(() => {
		const names = new Map<string, string>();
		for (const s of scanners.data ?? [])
			if (s.label !== s.id) names.set(s.id, s.label);
		for (const p of plugins.data ?? [])
			if (p.title !== p.id) names.set(p.id, p.title);
		return (id: string) => names.get(id);
	}, [scanners.data, plugins.data]);
};
