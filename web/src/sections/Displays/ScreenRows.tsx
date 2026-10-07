// **Screens** — the host's monitors and the screens streamed to devices, one list (D-C). The map
// draws; the rows act. A monitor row carries its own settings (stream it, stays on); a streamed
// row carries its state and **Release**, which has no other home.
import { Monitor, MonitorPlay, Settings2 } from "lucide-react";
import type { FC, ReactNode } from "react";
import type {
	ApiDisplayInfo,
	ApiMonitorInfo,
	DisplayPolicy,
	EffectivePolicy,
} from "@/api/gen/model";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardTitle } from "@/components/ui/card";
import { Segmented } from "@/components/ui/segmented";
import { m } from "@/paraglide/messages";
import { stateLabel } from "./DesktopMap";

export const ScreenRows: FC<{
	monitors: readonly ApiMonitorInfo[];
	displays: readonly ApiDisplayInfo[];
	/** The pin the host actually has, env override included. */
	pinned: string | null;
	/** The host can honour a pin at all (`enforced` carries `capture_monitor`). */
	pinSupported: boolean;
	policy?: DisplayPolicy;
	/** In-force policy: only `exclusive` turns a monitor off. */
	effective?: EffectivePolicy;
	/** Devices with settings of their own, by the name their screen carries. */
	overlaid: readonly string[];
	busy?: boolean;
	releasing?: boolean;
	onPick: (connector: string | null) => void;
	/** Keep this monitor lit through an exclusive stream (KWin, Hyprland, sway only). */
	onKeepLit?: (connector: string, keep: boolean) => void;
	onRelease: (slot: number) => void;
}> = ({
	monitors,
	displays,
	pinned,
	pinSupported,
	policy,
	effective,
	overlaid,
	busy,
	releasing,
	onPick,
	onKeepLit,
	onRelease,
}) => {
	// Our own virtual displays show up as heads on KWin; they are the streamed rows already.
	const heads = monitors.filter((mon) => !mon.managed);
	// A pin naming a monitor the host does not have fails every session until it changes, so it
	// gets a row of its own and a warning.
	const danglingPin =
		pinned &&
		!monitors.some(
			(mon) => mon.connector.toLowerCase() === pinned.toLowerCase(),
		)
			? pinned
			: null;
	// `PUNKTFUNK_CAPTURE_MONITOR` outranks the stored policy: a pin from the unit's environment is
	// read-only here, not a control that silently loses.
	const envLocked = !!pinned && !!policy && policy.capture_monitor !== pinned;
	const locked = busy || envLocked;
	const exclusive = effective?.topology === "exclusive";
	const keptLit = new Set(
		(policy?.keep_monitors ?? []).map((c) => c.toLowerCase()),
	);
	const isPinned = (c: string) => pinned?.toLowerCase() === c.toLowerCase();

	return (
		<Card>
			<CardContent className="space-y-3">
				<CardTitle>
					<h2>{m.display_screens()}</h2>
				</CardTitle>
				{envLocked && (
					<p className="text-sm text-amber-600 dark:text-amber-500">
						{m.display_monitor_env_locked()}
					</p>
				)}
				{danglingPin && (
					<p className="text-sm text-destructive">
						{m.display_monitor_missing_warning()}
					</p>
				)}
				{heads.length + displays.length === 0 && !danglingPin ? (
					<p className="text-sm text-muted-foreground">
						{m.display_map_empty()}
					</p>
				) : (
					<ul className="divide-y">
						{danglingPin && (
							<Row
								icon={<Monitor className="size-4" />}
								title={danglingPin}
								detail={m.display_monitor_missing_hint()}
								badges={
									<Badge variant="destructive">
										{m.display_monitor_missing()}
									</Badge>
								}
								actions={
									<Button
										size="sm"
										variant="outline"
										disabled={locked}
										onClick={() => onPick(null)}
									>
										{m.display_stream_virtual()}
									</Button>
								}
							/>
						)}
						{heads.map((mon) => (
							<Row
								key={mon.connector}
								icon={<Monitor className="size-4" />}
								title={mon.connector}
								detail={`${mon.description} · ${mon.mode}`}
								badges={
									<>
										{mon.primary && (
											<Badge variant="secondary">
												{m.display_monitor_primary()}
											</Badge>
										)}
										{!mon.enabled && (
											<Badge variant="outline">
												{m.display_monitor_disabled()}
											</Badge>
										)}
									</>
								}
								actions={
									<>
										{exclusive &&
											mon.enabled &&
											!isPinned(mon.connector) &&
											(onKeepLit ? (
												<Segmented
													busy={busy}
													value={keptLit.has(mon.connector.toLowerCase())}
													options={[
														[true, m.display_q_monitors_extend()],
														[false, m.display_q_monitors_exclusive()],
													]}
													onPick={(keep) => onKeepLit(mon.connector, keep)}
												/>
											) : (
												<span className="text-sm text-muted-foreground">
													{m.display_turns_off()}
												</span>
											))}
										{/* A disabled head cannot be streamed; it stays listed so "why isn't my
										    monitor here?" has an answer. */}
										{pinSupported && mon.enabled && (
											<Button
												size="sm"
												variant={
													isPinned(mon.connector) ? "default" : "outline"
												}
												aria-pressed={isPinned(mon.connector)}
												disabled={locked}
												onClick={() =>
													onPick(isPinned(mon.connector) ? null : mon.connector)
												}
											>
												{isPinned(mon.connector)
													? m.display_map_streamed()
													: m.display_stream_this()}
											</Button>
										)}
									</>
								}
							/>
						))}
						{displays.map((d) => (
							<Row
								key={d.slot}
								icon={<MonitorPlay className="size-4" />}
								title={d.client ?? m.display_map_unnamed()}
								detail={`${d.mode} · ${stateLabel(d.state, d.expires_in_ms)}`}
								badges={
									d.client != null &&
									overlaid.includes(d.client) && (
										<Settings2
											className="size-3.5 text-muted-foreground"
											aria-label={m.display_device_settings()}
										/>
									)
								}
								actions={
									// An active screen belongs to a live session: ending it is session
									// control, so only a kept one offers Release.
									d.state !== "active" && (
										<Button
											size="sm"
											variant="outline"
											disabled={releasing}
											onClick={() => onRelease(d.slot)}
										>
											{m.display_release()}
										</Button>
									)
								}
							/>
						))}
					</ul>
				)}
			</CardContent>
		</Card>
	);
};

const Row: FC<{
	icon: ReactNode;
	title: string;
	detail: string;
	badges?: ReactNode;
	actions?: ReactNode;
}> = ({ icon, title, detail, badges, actions }) => (
	<li className="flex flex-wrap items-center gap-x-3 gap-y-2 py-3">
		<span className="text-muted-foreground">{icon}</span>
		<div className="min-w-0 flex-1 basis-48">
			<div className="flex flex-wrap items-center gap-2 font-medium">
				<span className="truncate">{title}</span>
				{badges}
			</div>
			<div className="truncate text-xs text-muted-foreground">{detail}</div>
		</div>
		<div className="flex flex-wrap items-center gap-2">{actions}</div>
	</li>
);
