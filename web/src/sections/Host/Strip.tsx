import { toast } from "@unom/ui/toast";
import type { FC, ReactNode } from "react";
import { useState } from "react";
import { useListActions } from "@/api/gen/actions/actions";
import type { ActionInfo } from "@/api/gen/model";
import type { AvailableCompositor } from "@/api/gen/model/availableCompositor";
import type { HostInfo } from "@/api/gen/model/hostInfo";
import { OsIcon } from "@/components/os-icon";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { RowActions } from "@/components/ui/menu";
import { m } from "@/paraglide/messages";
import { CopyRow } from "./CopyRow";
import { actionTitle, ConfirmDialog } from "./PowerCard";
import { type Update, updateLine } from "./UpdateCard";

/**
 * The top of Host: who this host is and how current, how a device reaches it, and power in a ⋯.
 * The read-only facts — identity, codecs, ports, compositors, audio wiring — fold under Details
 * (R4: a fact never takes a card above the fold).
 */
export const HostStrip: FC<{
	host: HostInfo;
	compositors?: AvailableCompositor[];
	update: Update;
	/** The audio wiring facts (Windows), folded with the rest. */
	audio?: ReactNode;
}> = ({ host: h, compositors = [], update, audio }) => {
	const actions = useListActions();
	const [confirming, setConfirming] = useState<ActionInfo | null>(null);
	const list = actions.data?.actions ?? [];
	const compositor = compositors.find((c) => c.default && c.available);
	const params = new URLSearchParams({ host: h.local_ip });
	if (h.fingerprint) params.set("fp", h.fingerprint);
	// No port: clients default to 9777, the same as typing the address by hand.
	const deepLink = `punktfunk://connect/${h.uniqueid}?${params}`;
	const s = update.state.data;
	return (
		<Card>
			<CardContent className="space-y-4">
				<div className="flex flex-wrap items-center gap-x-3 gap-y-2">
					<OsIcon os={h.os} className="size-5 shrink-0" />
					<span className="font-semibold">{h.hostname}</span>
					<span className="text-sm text-muted-foreground">
						{[h.app_version, updateLine(update), h.os_name, compositor?.label]
							.filter(Boolean)
							.join(" · ")}
					</span>
					<div className="ml-auto flex items-center gap-1">
						{s && !s.check_disabled && !update.applying && !s.job && (
							<Button
								variant="outline"
								size="sm"
								disabled={update.checkBusy}
								onClick={update.onCheck}
							>
								{update.checkBusy ? m.update_checking() : m.host_check_update()}
							</Button>
						)}
						{list.length > 0 && (
							<RowActions
								label={m.host_power_menu()}
								actions={list.map((a) => ({
									label: actionTitle(a),
									destructive: a.danger,
									disabled: !a.available,
									onSelect: () => setConfirming(a),
								}))}
							/>
						)}
					</div>
				</div>
				<div className="grid grid-cols-1 gap-3 md:grid-cols-2">
					<CopyRow label={m.connect_address()} value={h.local_ip} />
					<CopyRow label={m.connect_link()} value={deepLink} />
				</div>
				<details className="group">
					<summary className="cursor-pointer text-sm text-muted-foreground hover:text-foreground">
						{m.common_details()}
					</summary>
					<div className="mt-3 space-y-4 text-sm">
						<p className="max-w-prose text-muted-foreground">
							{m.connect_help()}
						</p>
						<dl className="grid gap-x-6 gap-y-2 sm:grid-cols-2">
							<Fact label={m.host_hostname()} value={h.hostname} />
							<Fact label={m.host_os()} value={`${h.os_name} (${h.os})`} />
							<Fact label={m.host_local_ip()} value={h.local_ip} />
							<Fact
								label={m.host_version()}
								value={`${h.app_version} (${h.version})`}
							/>
							<Fact label={m.host_abi()} value={String(h.abi_version)} />
							<Fact label={m.host_uniqueid()} value={h.uniqueid} mono />
						</dl>
						<Facts label={m.host_codecs()}>
							{h.codecs.map((c) => (
								<Badge key={c} variant="secondary">
									{c.toUpperCase()}
								</Badge>
							))}
						</Facts>
						<Facts label={m.host_ports()}>
							{Object.entries(h.ports).map(([k, v]) => (
								<Badge key={k} variant="outline" className="tabular-nums">
									{k.toUpperCase()} {v as number}
								</Badge>
							))}
						</Facts>
						{/* A Windows host drives the pf-vdisplay driver: no compositor backends. */}
						{compositors.length > 0 && (
							<Facts label={m.host_compositors()}>
								{compositors.map((c) => (
									<Badge
										key={c.id}
										variant={c.available ? "secondary" : "outline"}
										title={c.id}
									>
										{c.label}
										{c.default ? ` · ${m.compositor_default()}` : ""}
										{c.available ? "" : ` · ${m.compositor_unavailable()}`}
									</Badge>
								))}
							</Facts>
						)}
						{audio}
						{list
							.filter((a) => !a.available && a.unavailable_reason)
							.map((a) => (
								<p key={a.id} className="text-xs text-muted-foreground">
									{actionTitle(a)}: {a.unavailable_reason}
								</p>
							))}
					</div>
				</details>
			</CardContent>
			{confirming && (
				<ConfirmDialog
					action={confirming}
					onClose={() => setConfirming(null)}
					onAccepted={(a) => {
						setConfirming(null);
						toast.success(m.host_power_sent({ action: actionTitle(a) }));
					}}
				/>
			)}
		</Card>
	);
};

const Fact: FC<{ label: string; value: string; mono?: boolean }> = ({
	label,
	value,
	mono,
}) => (
	<div className="flex items-baseline justify-between gap-4">
		<dt className="text-muted-foreground">{label}</dt>
		<dd
			className={mono ? "truncate font-mono text-xs" : "font-medium"}
			title={value}
		>
			{value}
		</dd>
	</div>
);

const Facts: FC<{ label: string; children: ReactNode }> = ({
	label,
	children,
}) => (
	<div className="space-y-1.5">
		<p className="text-muted-foreground">{label}</p>
		<div className="flex flex-wrap gap-1.5">{children}</div>
	</div>
);
