// One device's display settings (design/web-console-overhaul.md §6.2): the host's questions, each
// with **Follow host** first; an overlay pins only what the operator picks. Only the axes the host
// acts on PER DEVICE are asked (`client_enforced`). The screen goes first, as one role question
// where the host acts on both the screen and the topology.
import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import type { FC } from "react";
import {
	getGetDisplaySettingsQueryKey,
	useDeleteDisplayClient,
	useGetDisplayMonitors,
	useGetDisplaySettings,
	useSetDisplayClient,
} from "@/api/gen/display/display";
import type {
	ApiMonitorInfo,
	ClientOverlay,
	DisplayTarget,
	EffectivePolicy,
	Topology,
} from "@/api/gen/model";
import { Button } from "@/components/ui/button";
import {
	Dialog,
	DialogContent,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { Segmented } from "@/components/ui/segmented";
import { m } from "@/paraglide/messages";
import { GRANT_DISPLAY } from "@/sections/Devices/access";
import { describePolicy } from "./describePolicy";
import {
	conflictLabel,
	identityLabel,
	keepLabel,
	type PolicyPatch,
	PolicyQuestions,
	type PolicyValues,
	Question,
	topologyLabel,
} from "./PolicyQuestions";
import { monitorOf, pinLocked, screenOf } from "./ScreenRows";

/**
 * The device row's short form: what this device does DIFFERENTLY, in a few words.
 *
 * Not the full sentence — that belongs in the sheet, where there is room for it. A row is
 * scanned, so it carries only the pinned axes, in the same words the sheet uses for them.
 */
export function overlaySummary(overlay: ClientOverlay | undefined): string {
	const pinned = overlay ? stripNulls(overlay) : {};
	const parts: string[] = [];
	if (pinned.topology) parts.push(topologyLabel(pinned.topology));
	if (pinned.mode_conflict) parts.push(conflictLabel(pinned.mode_conflict));
	if (pinned.identity) parts.push(identityLabel(pinned.identity));
	if (pinned.keep_alive) parts.push(keepLabel(pinned.keep_alive));
	const screen = screenOf(overlay);
	if (screen === "own") parts.push(m.display_own_screen());
	else if (screen) parts.push(m.display_mirrors({ connector: screen.monitor }));
	if (overlay?.max_mode)
		parts.push(m.display_capped({ mode: overlay.max_mode }));
	if (overlay?.scale) parts.push(`${overlay.scale}×`);
	return parts.length === 0 ? m.display_follows_host() : parts.join(" · ");
}

/** orval types an absent field as `null | T`; the overlay's own meaning of absent is "follow". */
function stripNulls(
	overlay: ClientOverlay,
): Partial<EffectivePolicy> & PolicyValues {
	const out: Record<string, unknown> = {};
	for (const [k, v] of Object.entries(overlay)) {
		if (v !== null && v !== undefined) out[k] = v;
	}
	return out as Partial<EffectivePolicy> & PolicyValues;
}

/**
 * The overlay a write sends: the stored one with `patch` laid over it, a `null` dropping that
 * pin. The host derives `capture_monitor` and `set_by` from `screen`; sent back, the alias
 * would re-pin a monitor the patch just cleared.
 */
export function nextOverlay(
	overlay: ClientOverlay,
	patch: ClientOverlay | PolicyPatch,
): ClientOverlay {
	const screen = screenOf(overlay);
	const next: Record<string, unknown> = {
		...stripNulls(overlay),
		...(screen && { screen }),
		...patch,
	};
	delete next.capture_monitor;
	delete next.set_by;
	for (const [k, v] of Object.entries(patch)) {
		if (v === null) delete next[k];
	}
	return next as ClientOverlay;
}

const FOLLOW = "follow";

/** A device's role among your screens: a screen of its own under this topology. */
const ROLES = {
	only: "exclusive",
	main: "primary",
	second: "extend",
} as const satisfies Record<string, Topology>;
type Role = keyof typeof ROLES;
const isRole = (v: string): v is Role => Object.hasOwn(ROLES, v);

const roleLabel = (r: Role): string =>
	({
		only: m.display_role_only(),
		main: m.display_role_main(),
		second: m.display_role_second(),
	})[r];

const roleHint = (r: Role): string =>
	({
		only: m.display_role_only_hint(),
		main: m.display_role_main_hint(),
		second: m.display_role_second_hint(),
	})[r];

/** The role `topology` gives a screen of the device's own; `auto` as this host resolves it. */
function roleOf(
	topology: Topology | null | undefined,
	auto: Topology | undefined,
): Role | undefined {
	const t = topology === "auto" ? auto : topology;
	return (Object.keys(ROLES) as Role[]).find((r) => ROLES[r] === t);
}

/**
 * The screen question's answers after **Follow host**: the three roles (`roles`) or its own
 * screen, then each monitor that can be streamed. The device's monitor stays listed when it is
 * off or gone, so the pick shows.
 */
export function screenChoices(
	monitors: readonly ApiMonitorInfo[],
	current: DisplayTarget | undefined,
	roles = false,
): (readonly [string, string])[] {
	const heads = monitors
		.filter((mon) => !mon.managed && mon.enabled)
		.map((mon) => mon.connector);
	const mine = monitorOf(current);
	if (mine && !heads.includes(mine)) heads.push(mine);
	return [
		...(roles
			? (Object.keys(ROLES) as Role[]).map((r) => [r, roleLabel(r)] as const)
			: ([["own", m.display_own_screen()]] as const)),
		...heads.map((c) => [c, c] as const),
	];
}

/**
 * The answer a row shows. With `roles`, a screen of its own reads by the row's topology, else
 * the host's, and a topology alone counts as a screen of its own unless the host streams a
 * monitor (`pin`): then the device streams that monitor and follows the host.
 */
export function screenAnswer(
	overlay: ClientOverlay,
	roles: boolean,
	host: { topology: Topology; auto?: Topology; pin: string | null },
): string {
	const screen = screenOf(overlay);
	const monitor = monitorOf(screen);
	if (monitor) return monitor;
	if (!roles) return screen ? "own" : FOLLOW;
	if (screen !== "own" && (!overlay.topology || host.pin)) return FOLLOW;
	return roleOf(overlay.topology ?? host.topology, host.auto) ?? FOLLOW;
}

/** What an answer writes: a role sets both fields, a monitor drops the topology, Follow host both. */
export function screenPatch(answer: string): ClientOverlay {
	if (answer === FOLLOW) return { screen: null, topology: null };
	if (answer === "own") return { screen: "own" };
	if (isRole(answer)) return { screen: "own", topology: ROLES[answer] };
	return { screen: { monitor: answer }, topology: null };
}

/**
 * Who answers the screen question: the env pin, else the operator for a device whose access
 * lacks Choose its screen, else the device when it wrote its row. `undefined` grants is a host
 * that reports no access: its device rows read as before.
 */
export function screenDecider(
	locked: boolean,
	setBy: ClientOverlay["set_by"],
	grants: number | null | undefined,
): "env" | "operator" | "device" | undefined {
	if (locked) return "env";
	if (grants != null && (grants & GRANT_DISPLAY) === 0) return "operator";
	return setBy === "device" ? "device" : undefined;
}

export const ClientPolicySheet: FC<{
	open: boolean;
	onOpenChange: (open: boolean) => void;
	/** Pairing fingerprint — what the overlay is keyed by, never an address. */
	fingerprint: string;
	deviceName: string;
	/** The device's access mask; `null` is full, `undefined` a host that reports none. */
	grants?: number | null;
	/** Opens the device's access editor. */
	onEditAccess?: () => void;
}> = ({
	open,
	onOpenChange,
	fingerprint,
	deviceName,
	grants,
	onEditAccess,
}) => {
	const qc = useQueryClient();
	const settings = useGetDisplaySettings();
	const monitors = useGetDisplayMonitors();
	const save = useSetDisplayClient();
	const clear = useDeleteDisplayClient();

	const host = settings.data?.effective;
	const overlay: ClientOverlay = settings.data?.clients?.[fingerprint] ?? {};
	const enforced = settings.data?.client_enforced ?? [];
	const busy = save.isPending || clear.isPending;
	// What following the host streams, env pin included.
	const hostPin = monitors.data?.pinned ?? null;
	const locked = pinLocked(hostPin, settings.data?.settings);
	const screen = screenOf(overlay);
	const mirror = locked ? hostPin : screen ? monitorOf(screen) : hostPin;
	const decider = screenDecider(locked, overlay.set_by, grants);
	// One role question where the host acts on both fields; else the one it acts on, as asked alone.
	const roles = enforced.includes("screen") && enforced.includes("topology");
	const answer = host
		? screenAnswer(overlay, roles, {
				topology: host.topology,
				auto: settings.data?.auto_topology,
				pin: hostPin,
			})
		: FOLLOW;
	const followRole =
		roles && host && !hostPin
			? roleOf(host.topology, settings.data?.auto_topology)
			: undefined;
	const role =
		answer === FOLLOW ? followRole : isRole(answer) ? answer : undefined;
	// What the role in force does to your monitors, ahead of who decides it.
	const hint = role ? `${roleHint(role)} ` : "";

	const invalidate = () =>
		qc.invalidateQueries({ queryKey: getGetDisplaySettingsQueryKey() });

	/** The whole overlay is the unit of write: a field dropped here stops being pinned. */
	const write = (patch: ClientOverlay | PolicyPatch) =>
		save.mutate(
			{ fingerprint, data: nextOverlay(overlay, patch) },
			{
				onSuccess: () => {
					invalidate();
					toast.success(m.display_settings_saved());
				},
			},
		);

	const followAll = () =>
		clear.mutate(
			{ fingerprint },
			{
				onSuccess: () => {
					invalidate();
					toast.success(m.display_settings_saved());
				},
			},
		);

	const pinned = Object.keys(stripNulls(overlay)).length > 0;

	return (
		<Dialog open={open} onOpenChange={onOpenChange}>
			<DialogContent className="max-h-[85vh] overflow-y-auto sm:max-w-xl">
				<DialogHeader>
					<DialogTitle>{deviceName}</DialogTitle>
				</DialogHeader>

				{host && (
					<div className="space-y-5">
						<PolicyQuestions
							value={stripNulls(overlay)}
							axes={roles ? enforced.filter((a) => a !== "topology") : enforced}
							inherited={host}
							onSet={write}
							busy={busy}
							lead={
								enforced.includes("screen") && (
									<Question
										label={roles ? m.display_q_role() : m.display_q_screen()}
										help={
											decider === "env" ? (
												m.display_monitor_env_locked()
											) : decider === "operator" ? (
												<>
													{hint}
													{m.display_screen_you_decide({ name: deviceName })}
													{onEditAccess && (
														<>
															{" "}
															<Button
																variant="link"
																className="h-auto p-0 text-xs"
																onClick={onEditAccess}
															>
																{m.access_edit_title()}
															</Button>
														</>
													)}
												</>
											) : decider === "device" ? (
												`${hint}${m.display_screen_set_by_device()}`
											) : (
												role && roleHint(role)
											)
										}
									>
										<Segmented<string>
											busy={busy || locked}
											value={answer}
											options={[
												[
													FOLLOW,
													`${m.display_follow_host()} · ${hostPin ?? (followRole ? roleLabel(followRole) : m.display_own_screen())}`,
												],
												...screenChoices(
													monitors.data?.monitors ?? [],
													screen,
													roles,
												),
											]}
											onPick={(v) => write(screenPatch(v))}
										/>
									</Question>
								)
							}
						/>

						{/* The sentence this device will actually get, assembled by the same
						    generator as the host's — so the two can never disagree. */}
						<p className="rounded-md border bg-muted/40 p-3 text-sm">
							{describePolicy(
								{ ...host, ...stripNulls(overlay) },
								{ deviceName, mirror },
							)}
						</p>

						{pinned && (
							<Button variant="ghost" disabled={busy} onClick={followAll}>
								{m.display_follow_host_all()}
							</Button>
						)}
					</div>
				)}
			</DialogContent>
		</Dialog>
	);
};
