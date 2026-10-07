import { toast } from "@unom/ui/toast";
import { ChevronRight } from "lucide-react";
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
import { MenuItem, MenuSeparator, RowMenu } from "@/components/ui/menu";
import { m } from "@/paraglide/messages";
import { CopyRow } from "./CopyRow";
import { actionTitle, ConfirmDialog } from "./PowerCard";
import { type Update, updateLine } from "./UpdateCard";

/**
 * The top of Host, in three bands: who this host is and how current, with **Check for update**
 * and the power actions in ⋯; how a device reaches it; and its facts folded under **Details**
 * (R4: a fact never takes a card above the fold).
 */
export const HostStrip: FC<{
	host: HostInfo;
	compositors?: AvailableCompositor[];
	update: Update;
	/** The audio wiring facts (Windows), one cell of Details. */
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
			<CardContent className="divide-y">
				{/* The name keeps its width: on a phone the buttons drop under it. */}
				<div className="flex flex-wrap items-start gap-x-3 gap-y-2 pb-4">
					<OsIcon os={h.os} className="mt-0.5 size-6 shrink-0" />
					<div className="min-w-0 flex-1 basis-40">
						<h2 className="truncate text-lg font-semibold leading-tight">
							{h.hostname}
						</h2>
						<p className="text-sm text-muted-foreground">
							{[h.version, updateLine(update), h.os_name, compositor?.label]
								.filter(Boolean)
								.join(" · ")}
						</p>
					</div>
					<div className="ml-auto flex shrink-0 items-center gap-1">
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
						{/* Rare, and three of them end every stream: a menu, not a row of buttons. */}
						{list.length > 0 && (
							<RowMenu label={m.host_power_menu()}>
								{list.map((a, i) => (
									<ActionItem
										key={a.id}
										action={a}
										first={i > 0 && list[i - 1]?.group !== a.group}
										onSelect={() => setConfirming(a)}
									/>
								))}
							</RowMenu>
						)}
					</div>
				</div>

				<div className="space-y-2 py-4">
					<div className="grid grid-cols-1 gap-3 md:grid-cols-2">
						<CopyRow label={m.connect_address()} value={h.local_ip} />
						<CopyRow label={m.connect_link()} value={deepLink} />
					</div>
					<p className="text-xs text-muted-foreground">{m.connect_help()}</p>
				</div>

				<details className="group pt-4">
					<summary className="flex cursor-pointer list-none items-center gap-1.5 text-sm font-medium marker:content-none [&::-webkit-details-marker]:hidden">
						<ChevronRight className="size-4 text-muted-foreground transition-transform group-open:rotate-90" />
						{m.common_details()}
					</summary>
					<dl className="mt-4 grid gap-x-8 gap-y-4 sm:grid-cols-2 lg:grid-cols-3">
						<Fact label={m.host_hostname()} value={h.hostname} />
						<Fact label={m.host_os()} value={`${h.os_name} (${h.os})`} />
						<Fact label={m.host_version()} value={h.version} />
						<Fact label={m.host_local_ip()} value={h.local_ip} />
						<Fact label={m.host_abi()} value={String(h.abi_version)} />
						<Fact label={m.host_uniqueid()} value={h.uniqueid} mono />
						{h.gamestream && (
							<Fact label={m.host_gamestream_version()} value={h.app_version} />
						)}
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
					</dl>
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

/** One power action; one that this box cannot do says why, under its name. */
const ActionItem: FC<{
	action: ActionInfo;
	first: boolean;
	onSelect: () => void;
}> = ({ action: a, first, onSelect }) => (
	<>
		{first && <MenuSeparator />}
		<MenuItem
			destructive={a.danger}
			disabled={!a.available}
			onSelect={onSelect}
		>
			<span className="flex flex-col">
				{actionTitle(a)}
				{!a.available && a.unavailable_reason && (
					<span className="text-xs text-muted-foreground">
						{a.unavailable_reason}
					</span>
				)}
			</span>
		</MenuItem>
	</>
);

/** One fact: its name over its value, which truncates rather than widens the grid. */
const Fact: FC<{ label: string; value: string; mono?: boolean }> = ({
	label,
	value,
	mono,
}) => (
	<div className="min-w-0">
		<dt className="text-xs text-muted-foreground">{label}</dt>
		<dd
			className={
				mono ? "truncate font-mono text-xs" : "truncate text-sm font-medium"
			}
			title={value}
		>
			{value}
		</dd>
	</div>
);

/** A fact with several values, as badges, across the grid. */
export const Facts: FC<{ label: ReactNode; children: ReactNode }> = ({
	label,
	children,
}) => (
	<div className="col-span-full">
		<dt className="text-xs text-muted-foreground">{label}</dt>
		<dd className="mt-1.5 flex flex-wrap gap-1.5">{children}</dd>
	</div>
);
