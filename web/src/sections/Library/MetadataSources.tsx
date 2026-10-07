import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { ArrowDown, ArrowUp, Check, Images, Settings2 } from "lucide-react";
import { motion } from "motion/react";
import { type FC, useState } from "react";
import {
	getListLibraryMetadataQueryKey,
	useListLibraryMetadata,
	useSetLibraryMetadata,
} from "@/api/gen/library/library";
import type { CatalogEntry } from "@/api/gen/model";
import type { MetadataSourceInfo } from "@/api/gen/model/metadataSourceInfo";
import { useGetPluginCatalog } from "@/api/gen/store/store";
import { useSourceStatus } from "@/api/metadata";
import { METADATA_CATEGORY, usePlugins } from "@/api/plugins";
import { useInstallPlugin } from "@/api/store";
import { ROW, ROW_GAP, Stagger } from "@/components/stagger";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { apiErrorMessage } from "@/lib/errors";
import { m } from "@/paraglide/messages";
import { AddSourceRail, SourceGroup } from "./AddSource";
import { refreshLibrary } from "./helpers";
import { SourceSettingsDialog } from "./SourceSettings";
import { useSourceNames } from "./Sources";

/**
 * Art & Metadata: plugins that fill covers and details for games other sources list. The host
 * keeps their order and switches; each row reads its status line from the plugin itself.
 * The host lists a source only after its first push, so an installed plugin that never pushed
 * gets a plain row here; without it, a plugin that fails to start hides the whole card.
 */
export const MetadataSourcesSection: FC = () => {
	const qc = useQueryClient();
	const list = useListLibraryMetadata();
	const set = useSetLibraryMetadata();
	const plugins = usePlugins();
	const catalog = useGetPluginCatalog();
	const install = useInstallPlugin();
	const nameOf = useSourceNames();
	const [settingsFor, setSettingsFor] = useState<string | null>(null);

	const installed = new Set(
		(catalog.data?.plugins ?? [])
			.filter((p) => p.installed_version)
			.map((p) => p.pkg),
	);
	const available = (catalog.data?.plugins ?? []).filter(
		(p) =>
			p.categories?.includes(METADATA_CATEGORY) &&
			!installed.has(p.pkg) &&
			p.compatible,
	);
	const sources = list.data ?? [];
	const silent = (catalog.data?.plugins ?? []).filter(
		(p) =>
			p.categories?.includes(METADATA_CATEGORY) &&
			installed.has(p.pkg) &&
			!sources.some((s) => s.id === p.id),
	);
	if (
		!list.data ||
		(sources.length === 0 && available.length === 0 && silent.length === 0)
	) {
		return null;
	}
	const running = new Set((plugins.data ?? []).map((p) => p.id));

	const apply = async (next: MetadataSourceInfo[]) => {
		try {
			const out = await set.mutateAsync({
				data: next.map(({ id, enabled, replace }) => ({
					id,
					enabled,
					replace,
				})),
			});
			qc.setQueryData(getListLibraryMetadataQueryKey(), out);
			await refreshLibrary(qc);
		} catch (e) {
			toast.error(apiErrorMessage(e) ?? m.library_metadata_failed());
		}
	};
	const move = (i: number, by: -1 | 1) => {
		const next = [...sources];
		const [row] = next.splice(i, 1);
		if (!row) return;
		next.splice(i + by, 0, row);
		apply(next);
	};
	const patch = (i: number, change: Partial<MetadataSourceInfo>) =>
		apply(sources.map((s, j) => (j === i ? { ...s, ...change } : s)));

	const onInstall = async (entry: CatalogEntry) => {
		try {
			await install.mutateAsync({ source: entry.source, id: entry.id });
			toast.success(m.library_source_installing({ title: entry.title }));
		} catch (e) {
			toast.error(apiErrorMessage(e) ?? m.library_source_install_failed());
		}
	};

	const labelOf = (id: string) => nameOf(id) ?? id;
	return (
		<>
			<SourceGroup
				icon={<Images className="size-4" />}
				title={m.library_metadata_title()}
			>
				<div className="space-y-4">
					<Stagger gap={ROW_GAP} className="flex flex-col divide-y">
						{sources.map((source, i) => (
							<MetadataSourceRow
								key={source.id}
								source={source}
								label={labelOf(source.id)}
								running={running.has(source.id)}
								busy={set.isPending}
								first={i === 0}
								last={i === sources.length - 1}
								onToggle={() => patch(i, { enabled: !source.enabled })}
								onReplace={(replace) => patch(i, { replace })}
								onUp={() => move(i, -1)}
								onDown={() => move(i, 1)}
								onSettings={() => setSettingsFor(source.id)}
							/>
						))}
						{silent.map((entry) => (
							<motion.div
								key={entry.pkg}
								variants={ROW}
								className="flex flex-wrap items-center gap-3 py-3"
							>
								<span className="text-sm font-medium">{entry.title}</span>
								<Badge
									variant={running.has(entry.id) ? "secondary" : "outline"}
								>
									{running.has(entry.id)
										? m.library_source_running()
										: m.library_source_stopped()}
								</Badge>
								<span className="text-sm text-muted-foreground">
									{m.library_metadata_silent()}
								</span>
							</motion.div>
						))}
					</Stagger>
					<p className="max-w-prose text-xs text-muted-foreground">
						{m.library_metadata_help()}
					</p>
					<AddSourceRail
						entries={available}
						busy={catalog.data?.busy === true || install.isPending}
						onInstall={onInstall}
					/>
				</div>
			</SourceGroup>
			{settingsFor && (
				<SourceSettingsDialog
					source={{ id: settingsFor, label: labelOf(settingsFor) }}
					onClose={() => setSettingsFor(null)}
				/>
			)}
		</>
	);
};

const MetadataSourceRow: FC<{
	source: MetadataSourceInfo;
	label: string;
	running: boolean;
	busy: boolean;
	first: boolean;
	last: boolean;
	onToggle: () => void;
	onReplace: (replace: boolean) => void;
	onUp: () => void;
	onDown: () => void;
	onSettings: () => void;
}> = ({
	source,
	label,
	running,
	busy,
	first,
	last,
	onToggle,
	onReplace,
	onUp,
	onDown,
	onSettings,
}) => {
	const status = useSourceStatus(source.id, running);
	const replaceId = `metadata-replace-${source.id}`;
	return (
		<motion.div
			variants={ROW}
			className="flex flex-wrap items-center gap-3 py-3"
		>
			<Button
				size="sm"
				variant={source.enabled ? "default" : "outline"}
				aria-pressed={source.enabled}
				disabled={busy}
				onClick={onToggle}
			>
				{source.enabled && <Check className="size-4" />}
				{label}
			</Button>
			<Badge variant={running ? "secondary" : "outline"}>
				{running ? m.library_source_running() : m.library_source_stopped()}
			</Badge>
			{running && status.data && (
				<span
					className={`text-sm ${status.data.ready ? "text-muted-foreground" : "text-amber-600 dark:text-amber-500"}`}
				>
					{status.data.ready
						? m.library_metadata_found({
								found: status.data.found,
								wanted: status.data.wanted,
							})
						: status.data.reason}
				</span>
			)}
			<div className="ml-auto flex flex-wrap items-center gap-2">
				<label
					htmlFor={replaceId}
					className="flex items-center gap-2 text-sm"
					title={m.library_metadata_replace_help()}
				>
					<Checkbox
						id={replaceId}
						checked={source.replace}
						disabled={busy}
						onCheckedChange={(v) => onReplace(v === true)}
					/>
					{m.library_metadata_replace()}
				</label>
				<Button
					size="sm"
					variant="outline"
					aria-label={m.library_metadata_up()}
					disabled={busy || first}
					onClick={onUp}
				>
					<ArrowUp className="size-4" />
				</Button>
				<Button
					size="sm"
					variant="outline"
					aria-label={m.library_metadata_down()}
					disabled={busy || last}
					onClick={onDown}
				>
					<ArrowDown className="size-4" />
				</Button>
				<Button
					size="sm"
					variant="outline"
					aria-label={m.library_source_settings()}
					onClick={onSettings}
				>
					<Settings2 className="size-4" />
				</Button>
			</div>
		</motion.div>
	);
};
