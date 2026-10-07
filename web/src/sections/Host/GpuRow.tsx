import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import type { FC } from "react";
import {
	getListGpusQueryKey,
	useListGpus,
	useSetGpuPreference,
} from "@/api/gen/gpu/gpu";
import type { GpuState } from "@/api/gen/model";
import { Badge } from "@/components/ui/badge";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { apiErrorMessage } from "@/lib/errors";
import { m } from "@/paraglide/messages";

/** The automatic pick's value; a GPU id is never this. */
const AUTO = "-auto-";

/**
 * GPU preference as a row of Host → Video. A preference applies to the NEXT session. The host's
 * pins and warnings are the row's lock lines, as every setting's are.
 */
export const GpuRow: FC = () => {
	const qc = useQueryClient();
	// GPU state moves when a session starts or ends, which the event stream reports.
	const gpus = useListGpus({ query: { refetchInterval: 20_000 } });
	const setPref = useSetGpuPreference();
	const apply = (mode: "auto" | "manual", gpuId?: string) =>
		setPref.mutate(
			{ data: { mode, gpu_id: gpuId ?? null } },
			{
				onSuccess: () =>
					qc.invalidateQueries({ queryKey: getListGpusQueryKey() }),
				onError: (e) => toast.error(apiErrorMessage(e) ?? m.gpu_apply_failed()),
			},
		);
	const s = gpus.data;
	if (!s || s.gpus.length === 0) return null;
	return <GpuRowView state={s} busy={setPref.isPending} onApply={apply} />;
};

const fmtVram = (mb: number) =>
	mb >= 1024 ? `${Math.round(mb / 1024)} GiB` : `${mb} MiB`;

/**
 * The vendor an explicit `PUNKTFUNK_ENCODER` pin can open on (display name) — the console mirror
 * of the host's backend→vendor table. Vendor-agnostic and multi-vendor pins map to nothing.
 */
const encoderPinVendor: Record<string, string> = {
	nvenc: "NVIDIA",
	nvidia: "NVIDIA",
	cuda: "NVIDIA",
	hw: "NVIDIA",
	amf: "AMD",
	amd: "AMD",
	qsv: "Intel",
	intel: "Intel",
};

export const GpuRowView: FC<{
	state: GpuState;
	busy: boolean;
	onApply: (mode: "auto" | "manual", gpuId?: string) => void;
}> = ({ state: s, busy, onApply }) => {
	const pinVendor = s.encoder_pin ? encoderPinVendor[s.encoder_pin] : undefined;
	// The host overrides a pin whose vendor contradicts the next session's GPU; the stale pin
	// should go, so it is amber then and a quiet note otherwise.
	const pinConflict =
		pinVendor && s.selected && s.selected.vendor !== pinVendor.toLowerCase();
	return (
		<li className="flex flex-col gap-3 py-4 first:pt-1 last:pb-1 md:flex-row md:items-start md:justify-between md:gap-8">
			<div className="min-w-0 space-y-1 md:max-w-md">
				<div className="flex flex-wrap items-center gap-2">
					<span className="text-sm font-medium">{m.host_gpus()}</span>
					{s.active && (
						<Badge variant="success">
							{m.gpu_in_use({ backend: s.active.backend.toUpperCase() })}
						</Badge>
					)}
				</div>
				<p className="text-xs text-muted-foreground">{m.host_gpus_help()}</p>
				{s.selected?.source === "preference_missing" && (
					<p className="text-xs text-amber-600 dark:text-amber-500">
						{m.gpu_missing_warning({ name: s.preferred_name ?? "?" })}
					</p>
				)}
				{s.env_override && s.mode === "auto" && (
					<p className="text-xs text-muted-foreground">
						{m.gpu_env_note({ value: s.env_override })}
					</p>
				)}
				{s.encoder_pin &&
					(pinConflict && s.selected ? (
						<p className="text-xs text-amber-600 dark:text-amber-500">
							{m.gpu_encoder_pin_warning({
								value: s.encoder_pin,
								vendor: pinVendor ?? "",
								name: s.selected.name,
							})}
						</p>
					) : (
						<p className="text-xs text-muted-foreground">
							{m.gpu_encoder_pin_note({ value: s.encoder_pin })}
						</p>
					))}
			</div>
			<Select
				value={s.mode === "manual" && s.preferred_id ? s.preferred_id : AUTO}
				disabled={busy}
				onValueChange={(v) =>
					v === AUTO ? onApply("auto") : onApply("manual", v)
				}
			>
				<SelectTrigger className="md:w-72" aria-label={m.host_gpus()}>
					<SelectValue />
				</SelectTrigger>
				<SelectContent>
					<SelectItem value={AUTO}>
						{m.gpu_automatic()}
						{s.mode === "auto" && s.selected ? ` · ${s.selected.name}` : ""}
					</SelectItem>
					{s.gpus.map((g) => (
						<SelectItem key={g.id} value={g.id}>
							{g.name}
							{g.vram_mb > 0 ? ` · ${fmtVram(g.vram_mb)}` : ""}
						</SelectItem>
					))}
				</SelectContent>
			</Select>
		</li>
	);
};
