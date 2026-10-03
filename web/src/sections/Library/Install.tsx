import { toast } from "@unom/ui/toast";
import { AlertTriangle, CloudDownload, Pause } from "lucide-react";
import type { FC } from "react";
import { isLive, useDownloads, useInstallActions } from "@/api/downloads";
import type { Download } from "@/api/gen/model/download";
import type { Install } from "@/api/gen/model/install";
import { useDialogs } from "@/components/dialogs";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { apiErrorMessage } from "@/lib/errors";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";

/** Decimal units, as stores and file managers count: 12.3 GB, 48 MB. */
export const formatBytes = (n: number, locale: string): string => {
	const [value, unit] =
		n >= 1e9 ? [n / 1e9, "GB"] : n >= 1e6 ? [n / 1e6, "MB"] : [n / 1e3, "kB"];
	const digits = value >= 100 ? 0 : 1;
	return `${new Intl.NumberFormat(locale, { maximumFractionDigits: digits }).format(value)} ${unit}`;
};

const formatEta = (s: number): string => {
	if (s < 60) return m.library_install_eta_soon();
	const minutes = Math.round(s / 60);
	if (minutes < 60) return m.library_install_eta_min({ minutes });
	return m.library_install_eta_hours({
		hours: Math.floor(minutes / 60),
		minutes: minutes % 60,
	});
};

/** "12.3 GB of 26 GB · 48 MB/s · about 4 min left", as far as the row knows. */
export const progressLine = (d: Download, locale: string): string => {
	const parts: string[] = [
		d.total_bytes
			? m.library_install_progress({
					done: formatBytes(d.done_bytes, locale),
					total: formatBytes(d.total_bytes, locale),
				})
			: m.library_install_so_far({ done: formatBytes(d.done_bytes, locale) }),
	];
	if (d.state === "downloading" && d.rate_bps != null) {
		parts.push(
			m.library_install_rate({ rate: formatBytes(d.rate_bps, locale) }),
		);
	}
	if (d.state === "downloading" && d.eta_s != null)
		parts.push(formatEta(d.eta_s));
	return parts.join(" · ");
};

const stateLine = (d: Download, locale: string): string => {
	switch (d.state) {
		case "queued":
			return m.library_install_queued();
		case "installing":
			return d.phase ?? m.library_install_installing();
		case "paused":
			return `${m.library_install_paused()} · ${progressLine(d, locale)}`;
		case "failed":
			return d.error ?? m.library_install_failed_plain();
		case "done":
			return m.library_downloads_done();
		case "cancelled":
			return m.library_downloads_cancelled();
		default:
			return progressLine(d, locale);
	}
};

const percent = (d: Download): number | undefined =>
	d.total_bytes
		? Math.min(100, Math.floor((d.done_bytes / d.total_bytes) * 100))
		: undefined;

const Bar: FC<{ download: Download }> = ({ download }) => {
	const pct = percent(download);
	return (
		<div
			role="progressbar"
			aria-valuemin={0}
			aria-valuemax={100}
			{...(pct !== undefined ? { "aria-valuenow": pct } : {})}
			className="h-1.5 w-full overflow-hidden rounded-full bg-muted"
		>
			<div
				className={`h-full rounded-full bg-primary transition-[width] ${pct === undefined ? "w-1/3 animate-pulse" : ""}`}
				style={pct !== undefined ? { width: `${pct}%` } : undefined}
			/>
		</div>
	);
};

/** The tile's mark: what isn't installed, what downloads, what stopped. Nothing when installed. */
export const InstallBadge: FC<{ install?: Install; download?: Download }> = ({
	install,
	download,
}) => {
	const locale = useLocale();
	if (download && (isLive(download) || download.state === "paused")) {
		const pct = percent(download);
		const Icon = download.state === "paused" ? Pause : CloudDownload;
		return (
			<Badge
				variant="secondary"
				className="gap-1 bg-background/90 backdrop-blur"
			>
				<Icon className="size-3" />
				{pct !== undefined ? `${pct} %` : m.library_install_queued()}
			</Badge>
		);
	}
	if (download?.state === "failed") {
		return (
			<Badge
				variant="secondary"
				className="gap-1 bg-background/90 backdrop-blur"
			>
				<AlertTriangle className="size-3 text-destructive" />
				{m.library_badge_not_installed()}
			</Badge>
		);
	}
	if (install?.state !== "missing") return null;
	return (
		<Badge variant="outline" className="gap-1 bg-background/80 backdrop-blur">
			<CloudDownload className="size-3" />
			{install.size_bytes
				? formatBytes(install.size_bytes, locale)
				: m.library_badge_not_installed()}
		</Badge>
	);
};

/** Shared by the panel and the list: the action, a refusal toasted in the host's words. */
const useAct = () => {
	const actions = useInstallActions();
	const run = async (f: () => Promise<unknown>) => {
		try {
			await f();
		} catch (e) {
			toast.error(apiErrorMessage(e) ?? m.library_install_failed_toast());
		}
	};
	return { actions, run };
};

/** A title's install state and what can be done about it, on its page. */
export const InstallPanel: FC<{
	entry: { id: string; title: string; install?: Install | null } | null;
}> = ({ entry }) =>
	entry?.install ? (
		<InstallControls entry={{ ...entry, install: entry.install }} />
	) : null;

const InstallControls: FC<{
	entry: { id: string; title: string; install: Install };
}> = ({ entry }) => {
	const locale = useLocale();
	const { confirm } = useDialogs();
	const { actions, run } = useAct();
	const downloads = useDownloads();
	const d = downloads.data?.find((x) => x.app_id === entry.id);
	const busy =
		actions.install.isPending ||
		actions.pause.isPending ||
		actions.cancel.isPending ||
		actions.uninstall.isPending;
	const id = entry.id;
	const install = () => run(() => actions.install.mutateAsync({ id }));
	const cancel = async () => {
		const ok = await confirm({
			title: m.library_install_cancel_confirm(),
			description: m.library_install_cancel_body({
				size: formatBytes(d?.done_bytes ?? 0, locale),
			}),
			confirmLabel: m.library_install_cancel(),
			destructive: true,
		});
		if (ok) await run(() => actions.cancel.mutateAsync({ id }));
	};
	const remove = async () => {
		const ok = await confirm({
			title: m.library_install_remove_confirm({ title: entry.title }),
			description: m.library_install_remove_body({
				size: formatBytes(entry.install.size_bytes ?? 0, locale),
			}),
			confirmLabel: m.library_install_remove(),
			destructive: true,
		});
		if (ok) await run(() => actions.uninstall.mutateAsync({ id }));
	};

	const live = d && isLive(d);
	const paused = d?.state === "paused";
	const failed = d?.state === "failed";
	const missing = entry.install.state === "missing";
	const size = entry.install.size_bytes
		? formatBytes(entry.install.size_bytes, locale)
		: undefined;
	const free =
		entry.install.free_bytes != null
			? entry.install.target
				? m.library_install_free_in({
						free: formatBytes(entry.install.free_bytes, locale),
						folder: entry.install.target,
					})
				: m.library_install_free({
						free: formatBytes(entry.install.free_bytes, locale),
					})
			: undefined;

	return (
		<Card>
			<CardContent className="flex flex-col gap-3 sm:flex-row sm:items-center">
				<div className="min-w-0 flex-1 space-y-1.5">
					<p className="text-sm font-medium">
						{live || paused
							? stateLine(d, locale)
							: failed
								? m.library_install_failed({ error: d.error ?? "" })
								: missing
									? size
										? m.library_install_not_installed_size({ size })
										: m.library_badge_not_installed()
									: size
										? m.library_install_on_host({ size })
										: m.library_install_on_host_plain()}
					</p>
					{(live || paused) && <Bar download={d} />}
					{d?.phase && live && d.state !== "installing" && (
						<p className="text-xs text-muted-foreground">{d.phase}</p>
					)}
					{missing && !live && free && (
						<p className="break-all text-xs text-muted-foreground">{free}</p>
					)}
				</div>
				<div className="flex flex-wrap gap-2">
					{live && (
						<Button
							size="sm"
							variant="outline"
							disabled={busy}
							onClick={() => run(() => actions.pause.mutateAsync({ id }))}
						>
							{m.library_install_pause()}
						</Button>
					)}
					{(live || paused) && (
						<Button
							size="sm"
							variant="outline"
							disabled={busy}
							onClick={cancel}
						>
							{m.library_install_cancel()}
						</Button>
					)}
					{paused && (
						<Button size="sm" disabled={busy} onClick={install}>
							{m.library_install_resume()}
						</Button>
					)}
					{missing && !live && !paused && (
						<Button size="sm" disabled={busy} onClick={install}>
							<CloudDownload className="size-4" />
							{failed ? m.library_install_retry() : m.library_install_action()}
						</Button>
					)}
					{!missing && !live && !paused && (
						<Button
							size="sm"
							variant="outline"
							disabled={busy}
							onClick={remove}
						>
							{m.library_install_remove()}
						</Button>
					)}
				</div>
			</CardContent>
		</Card>
	);
};

/** Every download on the host: live ones first, then the last finished. Hidden when empty. */
export const DownloadsList: FC = () => {
	const locale = useLocale();
	const downloads = useDownloads();
	const { actions, run } = useAct();
	const rows = downloads.data ?? [];
	if (rows.length === 0) return null;
	const total = rows
		.filter((d) => d.state === "downloading")
		.reduce((n, d) => n + (d.rate_bps ?? 0), 0);
	return (
		<Card>
			<CardContent className="flex flex-col gap-3">
				<div className="flex items-center justify-between gap-2">
					<p className="text-sm font-medium">{m.library_downloads_title()}</p>
					{total > 0 && (
						<p className="text-xs text-muted-foreground">
							{m.library_downloads_total({ rate: formatBytes(total, locale) })}
						</p>
					)}
				</div>
				{rows.map((d) => (
					<div key={d.app_id} className="flex flex-col gap-1.5">
						<div className="flex items-center justify-between gap-3">
							<p className="min-w-0 truncate text-sm">{d.title}</p>
							<div className="flex shrink-0 gap-1">
								{isLive(d) && (
									<Button
										size="sm"
										variant="ghost"
										onClick={() =>
											run(() => actions.pause.mutateAsync({ id: d.app_id }))
										}
									>
										{m.library_install_pause()}
									</Button>
								)}
								{(d.state === "paused" || d.state === "failed") && (
									<Button
										size="sm"
										variant="ghost"
										onClick={() =>
											run(() => actions.install.mutateAsync({ id: d.app_id }))
										}
									>
										{d.state === "paused"
											? m.library_install_resume()
											: m.library_install_retry()}
									</Button>
								)}
							</div>
						</div>
						{(isLive(d) || d.state === "paused") && <Bar download={d} />}
						<p className="text-xs text-muted-foreground">
							{stateLine(d, locale)}
						</p>
					</div>
				))}
			</CardContent>
		</Card>
	);
};
