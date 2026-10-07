import { Link } from "@tanstack/react-router";
import { ArrowUpCircle, BadgeCheck, Ban, Circle, Package } from "lucide-react";
import { motion } from "motion/react";
import type { FC, ReactNode } from "react";
import type { InstalledView } from "@/api/gen/model";
import type { PluginAccessSnapshot } from "@/api/gen/model/pluginAccessSnapshot";
import { uiPlugins, usePlugins } from "@/api/plugins";
import { useInstalledPlugins } from "@/api/store";
import { QueryState } from "@/components/query-state";
import { ROW, ROW_GAP, Stagger } from "@/components/stagger";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { MenuItem, MenuSeparator, RowMenu } from "@/components/ui/menu";
import type { Loadable } from "@/lib/query";
import { m } from "@/paraglide/messages";
import {
	type AccessDecision,
	RecordedAccess,
	usePluginAccess,
} from "@/sections/PluginAccess";
import { RunnerCardSection } from "./Runner";
import { SourceChip, TierBadge } from "./TierBadge";

/**
 * Installed plugins plus their recorded folder access. This container owns both queries and the
 * access decisions; package updates and removals stay with the parent dialogs.
 */
export const InstalledTab: FC<{
	onUpdate: (plugin: InstalledView) => void;
	onUpdateAll: () => void;
	onUninstall: (plugin: InstalledView) => void;
	/** How many plugins "Update all" would install; the button hides at zero. */
	updateCount: number;
	/** Package whose install/uninstall is in flight, or null — only that row's actions disable. */
	busyPkg: string | null;
	/** An Update-all run is working through the queue — every action here waits for it. */
	batchRunning: boolean;
}> = ({
	onUpdate,
	onUpdateAll,
	onUninstall,
	updateCount,
	busyPkg,
	batchRunning,
}) => {
	const installed = useInstalledPlugins();
	const access = usePluginAccess();
	const plugins = usePlugins();
	return (
		<div className="flex flex-col gap-card">
			<RunnerCardSection />
			<InstalledList
				installed={installed}
				onUpdate={onUpdate}
				onUpdateAll={onUpdateAll}
				onUninstall={onUninstall}
				updateCount={updateCount}
				busyPkg={busyPkg}
				batchRunning={batchRunning}
				access={access.access.data}
				accessBusy={access.busy}
				onAccessDecision={access.onDecide}
				pages={new Set(uiPlugins(plugins.data).map((p) => p.id))}
			/>
		</div>
	);
};

const InstalledAccess: FC<{
	plugin: InstalledView;
	access: PluginAccessSnapshot[];
	busy: boolean;
	onDecision: (
		plugin: string,
		paths: string[],
		decision: AccessDecision,
	) => void;
}> = ({ plugin, access, busy, onDecision }) => {
	const id = plugin.plugin_id ?? plugin.entry_id;
	const snapshot = access.find((row) => row.plugin === id);
	if (!id || !snapshot) return null;
	return (
		<RecordedAccess
			access={snapshot}
			busy={busy}
			onDecide={(paths, decision) => onDecision(id, paths, decision)}
		/>
	);
};

/**
 * One installed-plugin row with provenance, runtime state, package actions, and recorded access.
 * Pending requests stay on the matching Library source; this list never announces them.
 */
export const InstalledList: FC<{
	installed: Loadable<InstalledView[]>;
	onUpdate: (plugin: InstalledView) => void;
	onUpdateAll: () => void;
	onUninstall: (plugin: InstalledView) => void;
	updateCount: number;
	busyPkg: string | null;
	batchRunning: boolean;
	access?: PluginAccessSnapshot[];
	accessBusy?: boolean;
	onAccessDecision?: (
		plugin: string,
		paths: string[],
		decision: AccessDecision,
	) => void;
	/** Plugin ids with a page of their own; their row's title opens it. */
	pages?: Set<string>;
}> = ({
	installed,
	onUpdate,
	onUpdateAll,
	onUninstall,
	updateCount,
	busyPkg,
	batchRunning,
	access = [],
	accessBusy = false,
	onAccessDecision = () => {},
	pages = new Set(),
}) => {
	const rows = installed.data ?? [];
	return (
		<Card>
			<CardContent flush>
				{/* The bulk action sits with the list it acts on, the way Sources' "Refresh all" does. */}
				<CardHeader className="flex-row flex-wrap items-center justify-between gap-3 space-y-0">
					<CardTitle className="flex items-center gap-2">
						<Package className="size-4" />
						{m.store_installed_title()}
					</CardTitle>
					{updateCount > 0 && (
						<Button size="sm" disabled={batchRunning} onClick={onUpdateAll}>
							<ArrowUpCircle className="size-4" />
							{m.store_update_all_count({ count: updateCount })}
						</Button>
					)}
				</CardHeader>

				<QueryState
					isLoading={installed.isLoading}
					error={installed.error}
					refetch={installed.refetch}
				>
					{rows.length === 0 ? (
						<p className="p-card pt-0 text-sm text-muted-foreground">
							{m.store_installed_empty()}
						</p>
					) : (
						<Stagger gap={ROW_GAP} className="divide-y px-card pb-2">
							{rows.map((p) => (
								<InstalledRow
									key={p.pkg}
									plugin={p}
									page={!!p.plugin_id && pages.has(p.plugin_id)}
									busy={batchRunning || busyPkg === p.pkg}
									onUpdate={() => onUpdate(p)}
									onUninstall={() => onUninstall(p)}
								>
									<InstalledAccess
										plugin={p}
										access={access}
										busy={accessBusy}
										onDecision={onAccessDecision}
									/>
								</InstalledRow>
							))}
						</Stagger>
					)}
				</QueryState>
			</CardContent>
		</Card>
	);
};

/**
 * One installed plugin: its name (a link when it has a page), then whether it runs, its version
 * and where it came from. Update shows when there is one; the rest is in ⋯. A verified package
 * is the usual case, so it gets a tick rather than a badge.
 */
const InstalledRow: FC<{
	plugin: InstalledView;
	page: boolean;
	busy: boolean;
	onUpdate: () => void;
	onUninstall: () => void;
	children?: ReactNode;
}> = ({ plugin: p, page, busy, onUpdate, onUninstall, children }) => {
	const title = p.title ?? p.pkg;
	return (
		<motion.div variants={ROW} className="py-3">
			<div className="flex items-center gap-3">
				<div className="min-w-0 flex-1" title={p.pkg}>
					<div className="flex min-w-0 items-center gap-2">
						{page && p.plugin_id ? (
							<Link
								to="/plugins/$pluginId/$"
								params={{ pluginId: p.plugin_id, _splat: "" }}
								className="truncate font-medium hover:underline"
							>
								{title}
							</Link>
						) : (
							<span className="truncate font-medium">{title}</span>
						)}
						{p.tier === "verified" ? (
							<span className="shrink-0" title={m.store_tier_verified_hint()}>
								<BadgeCheck
									className="size-4 text-[var(--success)]"
									aria-label={m.store_tier_verified()}
								/>
							</span>
						) : (
							<TierBadge tier={p.tier} className="shrink-0" />
						)}
					</div>
					<div className="flex min-w-0 items-center gap-1.5 text-xs text-muted-foreground">
						<Circle
							className={
								p.running
									? "size-2 shrink-0 fill-[var(--success)] text-[var(--success)]"
									: "size-2 shrink-0 fill-muted-foreground text-muted-foreground"
							}
						/>
						<span className="truncate">
							{p.running ? m.store_running() : m.store_stopped()} ·{" "}
							{p.version ? `v${p.version}` : m.store_version_unknown()}
						</span>
						{p.tier === "external" && p.source && (
							<SourceChip source={p.source} />
						)}
					</div>
				</div>
				{p.update_available != null && (
					<Button size="sm" disabled={busy} onClick={onUpdate}>
						<ArrowUpCircle className="size-4" />
						<span className="hidden sm:inline">
							{m.store_update_to({ version: p.update_available })}
						</span>
						<span className="sm:hidden">{p.update_available}</span>
					</Button>
				)}
				<RowMenu label={m.common_more_actions()} disabled={busy}>
					{page && p.plugin_id && (
						<>
							<MenuItem asChild>
								<Link
									to="/plugins/$pluginId/$"
									params={{ pluginId: p.plugin_id, _splat: "" }}
								>
									{m.store_open_page()}
								</Link>
							</MenuItem>
							<MenuSeparator />
						</>
					)}
					<MenuItem destructive onSelect={onUninstall}>
						{m.store_uninstall()}
					</MenuItem>
				</RowMenu>
			</div>
			{p.blocked != null && (
				<p className="mt-2 flex items-start gap-2 rounded-md border border-destructive/40 bg-destructive/10 px-2 py-1 text-xs font-medium text-destructive">
					<Ban className="mt-px size-3.5 shrink-0" />
					<span>{m.store_blocked({ reason: p.blocked })}</span>
				</p>
			)}
			{children}
		</motion.div>
	);
};
