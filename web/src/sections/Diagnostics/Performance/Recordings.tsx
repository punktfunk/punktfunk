import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { Download, Eye, HistoryIcon, Trash2 } from "lucide-react";
import type { FC } from "react";
import type { CaptureMeta } from "@/api/gen/model/captureMeta";
import {
	getStatsRecordingsListQueryKey,
	statsRecordingGet,
	useStatsRecordingDelete,
	useStatsRecordingsList,
} from "@/api/gen/stats/stats";
import { useDialogs } from "@/components/dialogs";
import { QueryState } from "@/components/query-state";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
	RowDetails,
	Table,
	TableBody,
	TableCell,
	TableHead,
	TableHeader,
	TableRow,
	WIDE,
} from "@/components/ui/table";
import { apiErrorMessage } from "@/lib/errors";
import { fmtClockDuration } from "@/lib/format";
import type { Loadable } from "@/lib/query";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";
import { fmtTimestamp, kindLabel } from "./helpers";

/**
 * Container: the saved recordings. Owns the list query, delete, and the JSON export. Selection is
 * the parent's UI state (it also drives the detail card), passed through here for row highlight +
 * to clear it when the selected recording is deleted.
 */
export const RecordingsSection: FC<{
	selectedId: string | null;
	onSelect: (id: string | null) => void;
}> = ({ selectedId, onSelect }) => {
	const qc = useQueryClient();
	const { confirm } = useDialogs();
	const recordings = useStatsRecordingsList();
	const del = useStatsRecordingDelete();

	const onDelete = async (id: string) => {
		const ok = await confirm({
			title: m.stats_delete_confirm(),
			description: m.stats_delete_body(),
			confirmLabel: m.stats_delete(),
			destructive: true,
		});
		if (!ok) return;
		del.mutate(
			{ id },
			{
				onSuccess: () => {
					if (selectedId === id) onSelect(null);
					qc.invalidateQueries({ queryKey: getStatsRecordingsListQueryKey() });
				},
				onError: (e) =>
					toast.error(apiErrorMessage(e) ?? m.stats_delete_failed()),
			},
		);
	};

	// Export the full Capture JSON via a one-off GET → blob download.
	const onDownload = async (id: string) => {
		try {
			const cap = await statsRecordingGet(id);
			const blob = new Blob([JSON.stringify(cap, null, 2)], {
				type: "application/json",
			});
			const url = URL.createObjectURL(blob);
			const a = document.createElement("a");
			a.href = url;
			a.download = `${id}.json`;
			document.body.appendChild(a);
			a.click();
			a.remove();
			URL.revokeObjectURL(url);
		} catch (e) {
			// The old comment claimed the detail view surfaces this — it only does so for the SELECTED
			// recording, and Download is offered on every row. Downloading an unselected one that
			// failed produced a button that visibly did nothing.
			toast.error(apiErrorMessage(e) ?? m.stats_download_failed());
		}
	};

	return (
		<RecordingsCard
			recordings={recordings}
			selectedId={selectedId}
			onSelect={onSelect}
			onDownload={onDownload}
			onDelete={onDelete}
			isDeleting={del.isPending}
		/>
	);
};

/** Saved recordings, with View / Download / Delete row actions. */
export const RecordingsCard: FC<{
	recordings: Loadable<CaptureMeta[]>;
	selectedId: string | null;
	onSelect: (id: string | null) => void;
	onDownload: (id: string) => void;
	onDelete: (id: string) => void;
	isDeleting: boolean;
}> = ({
	recordings,
	selectedId,
	onSelect,
	onDownload,
	onDelete,
	isDeleting,
}) => {
	const rows = recordings.data ?? [];
	return (
		<Card>
			<CardHeader>
				<CardTitle>
					<h2 className="flex items-center gap-2">
						<HistoryIcon className="size-4" />
						{m.stats_recordings_title()}
					</h2>
				</CardTitle>
			</CardHeader>
			<QueryState
				isLoading={recordings.isLoading}
				error={recordings.error}
				refetch={recordings.refetch}
			>
				{rows.length === 0 ? (
					<CardContent
						flush
						className="p-8 text-center text-sm text-muted-foreground"
					>
						{m.stats_recordings_empty()}
					</CardContent>
				) : (
					<CardContent flush>
						<Table>
							<TableHeader>
								<TableRow>
									<TableHead>{m.stats_col_time()}</TableHead>
									<TableHead>{m.stats_col_kind()}</TableHead>
									<TableHead className={WIDE}>
										{m.stats_col_resolution()}
									</TableHead>
									<TableHead className={WIDE}>{m.stats_col_codec()}</TableHead>
									<TableHead className={WIDE}>
										{m.stats_col_encoder()}
									</TableHead>
									<TableHead className={cn(WIDE, "text-right")}>
										{m.stats_col_duration()}
									</TableHead>
									<TableHead className={cn(WIDE, "text-right")}>
										{m.stats_col_samples()}
									</TableHead>
									<TableHead className="w-32" />
								</TableRow>
							</TableHeader>
							<TableBody>
								{rows.map((r) => (
									<TableRow
										key={r.id}
										data-state={selectedId === r.id ? "selected" : undefined}
									>
										<TableCell className="whitespace-nowrap font-medium">
											{fmtTimestamp(r.started_unix_ms)}
											<RowDetails>
												{r.width}×{r.height}@{r.fps} · {r.codec.toUpperCase()} ·{" "}
												{r.encoder_backend || "—"} ·{" "}
												{fmtClockDuration(r.duration_ms / 1000)}
											</RowDetails>
										</TableCell>
										<TableCell>
											<Badge
												variant={
													r.kind === "gamestream" ? "secondary" : "default"
												}
											>
												{kindLabel(r.kind)}
											</Badge>
										</TableCell>
										<TableCell
											className={cn(WIDE, "tabular-nums text-muted-foreground")}
										>
											{r.width}×{r.height}@{r.fps}
										</TableCell>
										<TableCell
											className={cn(WIDE, "uppercase text-muted-foreground")}
										>
											{r.codec}
										</TableCell>
										{/* The stage names and their meaning follow the backend: `driver-*`
										    records the Windows driver's stages. */}
										<TableCell
											className={cn(
												WIDE,
												"max-w-48 truncate text-muted-foreground",
											)}
											title={r.gpu || undefined}
										>
											{r.encoder_backend || "—"}
										</TableCell>
										<TableCell className={cn(WIDE, "text-right tabular-nums")}>
											{fmtClockDuration(r.duration_ms / 1000)}
										</TableCell>
										<TableCell className={cn(WIDE, "text-right tabular-nums")}>
											{r.sample_count}
										</TableCell>
										<TableCell>
											<div className="flex justify-end gap-1">
												<Button
													variant="ghost"
													size="icon"
													aria-label={m.stats_view()}
													title={m.stats_view()}
													onClick={() =>
														onSelect(selectedId === r.id ? null : r.id)
													}
												>
													<Eye className="size-4" />
												</Button>
												<Button
													variant="ghost"
													size="icon"
													aria-label={m.stats_download()}
													title={m.stats_download()}
													onClick={() => onDownload(r.id)}
												>
													<Download className="size-4" />
												</Button>
												<Button
													variant="ghost"
													size="icon"
													aria-label={m.stats_delete()}
													title={m.stats_delete()}
													disabled={isDeleting}
													onClick={() => onDelete(r.id)}
												>
													<Trash2 className="size-4 text-destructive" />
												</Button>
											</div>
										</TableCell>
									</TableRow>
								))}
							</TableBody>
						</Table>
					</CardContent>
				)}
			</QueryState>
		</Card>
	);
};
