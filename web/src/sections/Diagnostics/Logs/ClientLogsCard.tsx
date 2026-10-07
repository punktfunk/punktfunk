import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { ChevronDown, ChevronRight, Download, Trash2 } from "lucide-react";
import { type FC, useState } from "react";
import {
	clientLogsGet,
	getClientLogsListQueryKey,
	useClientLogsDelete,
} from "@/api/gen/logs/logs";
import type { ClientLogMeta } from "@/api/gen/model/clientLogMeta";
import { useDialogs } from "@/components/dialogs";
import { QueryState } from "@/components/query-state";
import { Button } from "@/components/ui/button";
import {
	Table,
	TableBody,
	TableCell,
	TableHead,
	TableHeader,
	TableRow,
} from "@/components/ui/table";
import { apiErrorMessage } from "@/lib/errors";
import type { Loadable } from "@/lib/query";
import { m } from "@/paraglide/messages";
import { fmtTimestamp } from "../Performance/helpers";

/** `123.4 KB` — bundles are ≤1 MiB, so two units cover the whole range. */
const fmtSize = (bytes: number): string =>
	bytes >= 1024 * 1024
		? `${(bytes / (1024 * 1024)).toFixed(1)} MB`
		: `${(bytes / 1024).toFixed(1)} KB`;

/**
 * Container for bundle housekeeping: fetch the raw file, delete one.
 *
 * Reading a bundle is no longer this component's job — the device chips merge it into the viewer
 * above. What is left is the file-cabinet half (keep the raw bytes, throw one away), which is
 * occasional and belongs behind a disclosure rather than in a card of its own competing with the
 * log for the top of the page.
 */
export const ClientLogsSection: FC<{
	list: Loadable<ClientLogMeta[]>;
	/** Drop a deleted bundle's rows from the viewer — the list alone cannot do that. */
	onDeleted: (id: string) => void;
}> = ({ list, onDeleted }) => {
	const qc = useQueryClient();
	const { confirm } = useDialogs();
	const del = useClientLogsDelete();

	const onDelete = async (id: string) => {
		const ok = await confirm({
			title: m.client_logs_delete_confirm(),
			description: m.client_logs_delete_body(),
			confirmLabel: m.client_logs_delete(),
			destructive: true,
		});
		if (!ok) return;
		del.mutate(
			{ id },
			{
				onSuccess: () => {
					onDeleted(id);
					qc.invalidateQueries({ queryKey: getClientLogsListQueryKey() });
				},
				onError: (e) =>
					toast.error(apiErrorMessage(e) ?? m.client_logs_delete_failed()),
			},
		);
	};

	// Plain-text bundle → blob download, same shape as the recordings JSON export.
	const onDownload = async (id: string) => {
		try {
			const text = await clientLogsGet(id);
			const blob = new Blob([text], { type: "text/plain" });
			const url = URL.createObjectURL(blob);
			const a = document.createElement("a");
			a.href = url;
			a.download = `${id}.log`;
			document.body.appendChild(a);
			a.click();
			a.remove();
			URL.revokeObjectURL(url);
		} catch (e) {
			toast.error(apiErrorMessage(e) ?? m.client_logs_download_failed());
		}
	};

	return (
		<ClientLogsCard
			bundles={list}
			onDownload={onDownload}
			onDelete={onDelete}
			isDeleting={del.isPending}
		/>
	);
};

/**
 * Uploaded bundles as a collapsed disclosure: Download (raw) / Delete per row.
 *
 * Collapsed by default because it answers a question nobody arrives with. It renders nothing at all
 * when there is nothing stored — the "no device has sent logs yet" hint now lives beside the source
 * chips, where someone who has never used the feature will actually meet it.
 */
export const ClientLogsCard: FC<{
	bundles: Loadable<ClientLogMeta[]>;
	onDownload: (id: string) => void;
	onDelete: (id: string) => void;
	isDeleting: boolean;
}> = ({ bundles, onDownload, onDelete, isDeleting }) => {
	const [open, setOpen] = useState(false);
	const rows = bundles.data ?? [];
	// Errors and loading still render: a broken list must not look like "nothing was sent".
	if (!bundles.isLoading && !bundles.error && rows.length === 0) return null;

	return (
		<div className="flex flex-col gap-2">
			<button
				type="button"
				className="flex items-center gap-1 self-start text-xs text-muted-foreground hover:text-foreground"
				aria-expanded={open}
				onClick={() => setOpen((v) => !v)}
			>
				{open ? (
					<ChevronDown className="size-3" />
				) : (
					<ChevronRight className="size-3" />
				)}
				{m.client_logs_manage({ count: rows.length })}
			</button>

			{open && (
				<QueryState
					isLoading={bundles.isLoading}
					error={bundles.error}
					refetch={bundles.refetch}
				>
					<div className="rounded-md border">
						<Table>
							<TableHeader>
								<TableRow>
									<TableHead>{m.client_logs_col_received()}</TableHead>
									<TableHead>{m.client_logs_col_device()}</TableHead>
									<TableHead className="text-right">
										{m.client_logs_col_size()}
									</TableHead>
									<TableHead className="w-24" />
								</TableRow>
							</TableHeader>
							<TableBody>
								{rows.map((r) => (
									<TableRow key={r.id}>
										<TableCell className="whitespace-nowrap font-medium">
											{fmtTimestamp(r.received_ms)}
										</TableCell>
										<TableCell>
											<span>{r.device_name}</span>
											<span className="ml-2 font-mono text-xs text-muted-foreground">
												{r.fingerprint_prefix}
											</span>
										</TableCell>
										<TableCell className="text-right tabular-nums">
											{fmtSize(r.size_bytes)}
										</TableCell>
										<TableCell>
											<div className="flex justify-end gap-1">
												<Button
													variant="ghost"
													size="icon"
													aria-label={m.client_logs_download()}
													title={m.client_logs_download()}
													onClick={() => onDownload(r.id)}
												>
													<Download className="size-4" />
												</Button>
												<Button
													variant="ghost"
													size="icon"
													aria-label={m.client_logs_delete()}
													title={m.client_logs_delete()}
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
					</div>
				</QueryState>
			)}
		</div>
	);
};
