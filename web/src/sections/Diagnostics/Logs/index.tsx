import { toast } from "@unom/ui/toast";
import { Download } from "lucide-react";
import { type FC, useEffect, useMemo, useState } from "react";
import { ApiError } from "@/api/fetcher";
import { useGetDiagnostics } from "@/api/gen/diagnostics/diagnostics";
import { Button } from "@/components/ui/button";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";
import { ChecksSection } from "./ChecksCard";
import { ClientLogsSection } from "./ClientLogsCard";
import {
	detectShareMode,
	diagnosticsFilename,
	diagnosticsText,
	downloadText,
	logFilename,
	logsToText,
	type ShareMode,
	shareLogs,
} from "./export";
import { LogsCard, type SourceChoice } from "./LogsCard";
import {
	deviceSource,
	HOST_SOURCE,
	hostRows,
	mergeRows,
	PLUGINS_SOURCE,
} from "./rows";
import { useDeviceLogs, useHostLog } from "./useLogSources";
import { LogsView } from "./view";

/** `12:04` — a chip has room for when a bundle landed, not for the date it landed on. */
const fmtClock = (ms: number): string => {
	const d = new Date(ms);
	const p = (n: number) => String(n).padStart(2, "0");
	return `${p(d.getHours())}:${p(d.getMinutes())}`;
};

/**
 * Troubleshooting: the host's health checks over one viewer that holds every log this host can
 * reach — its own, the plugin runner's, and whatever paired devices have uploaded.
 *
 * The page owns the log state rather than the viewer card, because two consumers read it: the
 * viewer, and the "export everything" action in the heading.
 */
export const SectionLogs: FC = () => {
	useLocale();

	const host = useHostLog();
	const devices = useDeviceLogs();

	// Host and plugins on by default — the page's previous "All", and still the right opening
	// state: a device bundle is a deliberate act of correlation, not something to be opted out of.
	const [selected, setSelected] = useState<Set<string>>(
		() => new Set([HOST_SOURCE, PLUGINS_SOURCE]),
	);
	const [shareMode, setShareMode] = useState<ShareMode | null>(null);
	const [exporting, setExporting] = useState(false);

	// Probed after mount: the server render has no `navigator`, and guessing there would mismatch
	// on hydration. Until then the share button is simply absent.
	useEffect(() => {
		setShareMode(detectShareMode());
	}, []);

	// The checks are already on this page; the export reads the same cached entry rather than
	// asking the host to run every probe a second time.
	const diagnostics = useGetDiagnostics({
		query: { staleTime: 5 * 60_000, retry: false },
	});
	const checksUnsupported =
		diagnostics.error instanceof ApiError && diagnostics.error.status === 404;

	const fromHost = useMemo(() => hostRows(host.entries), [host.entries]);

	// Only SELECTED device bundles reach the merge. An export loads every bundle as a side effect,
	// and without this the pool would silently grow by a few thousand rows per device that nobody
	// asked to see — sorted on every poll, for nothing.
	const rows = useMemo(
		() =>
			mergeRows([
				fromHost,
				...devices.bundles
					.filter((b) => selected.has(deviceSource(b.meta.id)))
					.map((b) => b.rows),
			]),
		[fromHost, devices.bundles, selected],
	);

	const sources = useMemo<SourceChoice[]>(
		() => [
			{
				id: HOST_SOURCE,
				label: m.logs_source_host(),
				selected: selected.has(HOST_SOURCE),
			},
			{
				id: PLUGINS_SOURCE,
				label: m.logs_source_plugins(),
				selected: selected.has(PLUGINS_SOURCE),
			},
			...devices.bundles.map((b) => ({
				id: deviceSource(b.meta.id),
				label: b.meta.device_name,
				hint: fmtClock(b.meta.received_ms),
				selected: selected.has(deviceSource(b.meta.id)),
				state:
					b.state === "loading"
						? ("loading" as const)
						: b.state === "error"
							? ("error" as const)
							: undefined,
			})),
		],
		[devices.bundles, selected],
	);

	const onToggleSource = (id: string) => {
		const turningOn = !selected.has(id);
		setSelected((prev) => {
			const next = new Set(prev);
			if (next.has(id)) next.delete(id);
			else next.add(id);
			return next;
		});
		// Switching a device on is what asks for its bundle — including after a failure, so a
		// second click is a retry rather than a no-op.
		if (!turningOn) return;
		const bundle = devices.bundles.find((b) => deviceSource(b.meta.id) === id);
		if (bundle && bundle.state !== "loaded" && bundle.state !== "loading") {
			void devices.load(bundle.meta);
		}
	};

	const onExportAll = async () => {
		setExporting(true);
		try {
			const bundles = await devices.loadAll();
			downloadText(
				diagnosticsText({
					generatedAt: new Date(),
					checks: diagnostics.data?.checks ?? [],
					checksUnavailable: checksUnsupported,
					// The full host buffer, not `rows` — the export is "everything", and what the
					// viewer is filtered to has no bearing on that.
					rows: fromHost,
					bundles,
				}),
				diagnosticsFilename(new Date()),
			);
		} catch {
			toast.error(m.logs_export_all_failed());
		} finally {
			setExporting(false);
		}
	};

	const devicesEmpty =
		!devices.list.isLoading &&
		!devices.list.error &&
		devices.bundles.length === 0;

	return (
		<LogsView
			checks={<ChecksSection />}
			actions={
				<Button variant="outline" onClick={onExportAll} disabled={exporting}>
					<Download className="size-4" />
					{exporting ? m.logs_export_all_working() : m.logs_export_all()}
				</Button>
			}
			viewer={
				<LogsCard
					rows={rows}
					sources={sources}
					onToggleSource={onToggleSource}
					devicesEmpty={devicesEmpty}
					manage={
						<ClientLogsSection list={devices.list} onDeleted={devices.forget} />
					}
					follow={host.follow}
					onFollow={host.setFollow}
					onClear={host.clear}
					onDownload={(shown) =>
						downloadText(logsToText(shown), logFilename(new Date()))
					}
					onShare={async (shown) => {
						const outcome = await shareLogs(
							logsToText(shown),
							logFilename(new Date()),
						);
						if (outcome === "copied") toast.success(m.logs_copied());
						else if (outcome === "failed") toast.error(m.logs_share_failed());
					}}
					shareMode={shareMode}
					dropped={host.dropped}
					error={host.error}
					isLoading={host.isLoading}
					onRetry={() => host.refetch()}
				/>
			}
		/>
	);
};
