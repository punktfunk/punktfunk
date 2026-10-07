// The **Displays** page (design/web-console-structure-2026-10.md §5.4): the owner's desktop, the
// only one with monitors to answer for.
//
// State above configuration: the map and the Screens rows say what is happening, the sentence and
// the presets say what happens next. The five-second test is "what happens to my monitors when a
// device connects" — the ghost on the idle map and the sentence ARE the answer.
//
// One persistence model: everything saves on change. The host applies at the next connect.

import { useQueryClient } from "@tanstack/react-query";
import Section from "@unom/ui/section";
import { toast } from "@unom/ui/toast";
import { type FC, useState } from "react";
import {
	getGetDisplayMonitorsQueryKey,
	getGetDisplaySettingsQueryKey,
	getGetDisplayStateQueryKey,
	useCreateCustomPreset,
	useDeleteCustomPreset,
	useGetDisplayMonitors,
	useGetDisplaySettings,
	useGetDisplayState,
	useReleaseDisplay,
	useSetDisplayLayout,
	useSetDisplaySettings,
	useUpdateCustomPreset,
} from "@/api/gen/display/display";
import type {
	CustomPreset,
	DisplayPolicy,
	EffectivePolicy,
	Topology,
} from "@/api/gen/model";
import { useListNativeClients } from "@/api/gen/native/native";
import { usePlatform } from "@/api/platform";
import { useDialogs } from "@/components/dialogs";
import { DocsLink } from "@/components/docs-link";
import { QueryState } from "@/components/query-state";
import { Stagger } from "@/components/stagger";
import { Card, CardContent, CardTitle } from "@/components/ui/card";
import { RowActions } from "@/components/ui/menu";
import { apiErrorMessage } from "@/lib/errors";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";
import { BehaviourPicker, CustomiseDialog } from "./Behaviour";
import { DesktopMap, ghostBox } from "./DesktopMap";
import { AdvancedDisclosure } from "./Disclosures";
import { describePolicy } from "./describePolicy";
import { OtherDesktops } from "./OtherDesktops";
import { ScreenRows } from "./ScreenRows";

export const SectionDisplays: FC = () => {
	useLocale();
	const qc = useQueryClient();
	const { confirm, promptText } = useDialogs();
	const { acts } = usePlatform();

	const settings = useGetDisplaySettings();
	const monitors = useGetDisplayMonitors();
	// Create/release arrive on the event stream, so the timer only covers the one thing events
	// cannot express: the per-second countdown on a kept display.
	const state = useGetDisplayState({
		query: {
			refetchInterval: (q) =>
				q.state.data?.displays?.some((d) => d.expires_in_ms != null)
					? 2_000
					: 15_000,
		},
	});
	const save = useSetDisplaySettings();
	const release = useReleaseDisplay();
	const saveLayout = useSetDisplayLayout();
	const createPreset = useCreateCustomPreset();
	const updatePreset = useUpdateCustomPreset();
	const deletePreset = useDeleteCustomPreset();

	const [customiseOpen, setCustomiseOpen] = useState(false);
	// What the map should show while a preset card is hovered — never written anywhere.
	const [preview, setPreview] = useState<EffectivePolicy | undefined>();

	const policy = settings.data?.settings;
	const effective = settings.data?.effective;
	const displays = state.data?.displays ?? [];
	const heads = monitors.data?.monitors ?? [];
	const live = displays.some((d) => d.state === "active");
	const kept = displays.filter((d) => d.state !== "active");
	const shown = preview ?? effective;
	const busy = save.isPending;
	const pinned = monitors.data?.pinned ?? null;
	// `auto` is the host's call (extend under a compositor pin, else exclusive); the host says
	// which, so the ghost and the dimming show the outcome rather than a guess.
	const concrete = (t?: Topology) =>
		t === "auto" ? settings.data?.auto_topology : t;
	const topology = concrete(shown?.topology);
	const ghost = topology ? ghostBox(heads, topology, !!pinned) : undefined;
	// A box on the map is labelled with the device's name; an overlay is keyed by its
	// fingerprint. The paired list is the only place both appear, so it is what turns one
	// into the other (§6.2).
	const paired = useListNativeClients();
	const overlaid = (paired.data ?? [])
		.filter((c) => settings.data?.clients?.[c.fingerprint])
		.map((c) => c.name);

	const invalidate = () => {
		qc.invalidateQueries({ queryKey: getGetDisplaySettingsQueryKey() });
		qc.invalidateQueries({ queryKey: getGetDisplayMonitorsQueryKey() });
	};

	/** The page's only policy write: the stored policy with some fields replaced. */
	const write = (patch: Partial<DisplayPolicy>) => {
		if (!policy) return;
		save.mutate(
			// `capture_monitor` is read back from the server on every write rather than carried
			// through a component: the monitor rows own it, and a stale copy here is exactly how
			// applying a preset used to un-pin a mirroring host.
			{ data: { ...policy, ...patch } },
			{
				onSuccess: () => {
					invalidate();
					toast.success(m.display_settings_saved());
				},
			},
		);
	};

	/** A preset field switches the policy to Custom, pinning what is in effect around it. */
	const writeField = (patch: Partial<DisplayPolicy>) =>
		write(
			policy?.preset === "custom"
				? patch
				: { preset: "custom", ...effective, ...patch },
		);

	const doRelease = (slot?: number) =>
		release.mutate(
			{ data: { slot: slot ?? null } },
			{
				onSuccess: () =>
					qc.invalidateQueries({ queryKey: getGetDisplayStateQueryKey() }),
			},
		);

	/** Drag-drop on the map: place one screen and switch the host to a manual layout. */
	const moveDisplay = (slot: number, x: number, y: number) => {
		const d = displays.find((it) => it.slot === slot);
		if (d?.identity_slot == null) return;
		// `PUT /display/layout` REPLACES the whole map, so every stored position has to ride
		// along or an offline device's saved placement is deleted.
		const positions = { ...(policy?.layout?.positions ?? {}) };
		positions[String(d.identity_slot)] = { x, y };
		saveLayout.mutate(
			{ data: { positions } },
			{
				onSuccess: () => {
					invalidate();
					qc.invalidateQueries({ queryKey: getGetDisplayStateQueryKey() });
				},
			},
		);
	};

	const savePreset = async () => {
		if (!effective) return;
		const name = (
			await promptText({
				title: m.display_preset_save_title(),
				label: m.display_preset_name(),
			})
		)?.trim();
		if (!name) return;
		createPreset.mutate(
			{
				data: {
					name,
					fields: effective,
					game_session: policy?.game_session ?? "auto",
				},
			},
			{ onSuccess: invalidate },
		);
	};

	const renamePreset = async (p: CustomPreset) => {
		const name = (
			await promptText({
				title: m.display_preset_edit(),
				label: m.display_preset_name(),
				defaultValue: p.name,
			})
		)?.trim();
		if (!name) return;
		updatePreset.mutate(
			{
				id: p.id,
				data: {
					name,
					fields: p.fields,
					game_session: p.game_session ?? "auto",
				},
			},
			{ onSuccess: invalidate },
		);
	};

	const removePreset = async (p: CustomPreset) => {
		const ok = await confirm({
			title: m.display_preset_delete_confirm(),
			confirmLabel: m.display_preset_delete(),
			destructive: true,
		});
		if (ok) deletePreset.mutate({ id: p.id }, { onSuccess: invalidate });
	};

	const error = apiErrorMessage(save.error ?? saveLayout.error);

	return (
		<Section maxWidth={false}>
			<div className="flex flex-col gap-card">
				<div className="flex flex-wrap items-center gap-3">
					<h1 className="text-2xl font-semibold">{m.nav_displays()}</h1>
					<DocsLink path="virtual-displays" className="text-sm" />
					{kept.length > 0 && (
						<div className="ml-auto">
							<RowActions
								disabled={release.isPending}
								actions={[
									{
										label: m.display_release_all(),
										onSelect: () => doRelease(),
									},
								]}
							/>
						</div>
					)}
				</div>

				{/* One group, mounted once both queries settle. Errors stay with each card. */}
				<QueryState
					isLoading={settings.isLoading || monitors.isLoading}
					error={undefined}
				>
					<Stagger className="flex flex-col gap-card">
						<Card>
							<CardContent className="space-y-3">
								<QueryState
									isLoading={settings.isLoading || monitors.isLoading}
									error={
										effective ? undefined : (settings.error ?? monitors.error)
									}
									refetch={settings.refetch}
								>
									{/* The map's own height, so a preview that empties it cannot move the page. */}
									{heads.length + displays.length === 0 && !ghost ? (
										<div className="flex h-48 items-center justify-center sm:h-72">
											<p className="text-sm text-muted-foreground">
												{m.display_map_empty()}
											</p>
										</div>
									) : (
										<DesktopMap
											monitors={heads}
											displays={displays}
											dimMonitors={topology === "exclusive"}
											keepLit={policy?.keep_monitors}
											ghost={ghost}
											overlaid={overlaid}
											captureMonitor={pinned}
											onMove={moveDisplay}
											busy={saveLayout.isPending}
										/>
									)}
									{displays.length > 1 && (
										<p className="text-xs text-muted-foreground">
											{m.display_arrange_hint()}
										</p>
									)}
									{error && <p className="text-sm text-destructive">{error}</p>}
								</QueryState>
							</CardContent>
						</Card>

						<ScreenRows
							monitors={heads}
							displays={displays}
							pinned={pinned}
							pinSupported={acts("display", "capture_monitor")}
							policy={policy}
							effective={effective}
							overlaid={overlaid}
							busy={busy}
							releasing={release.isPending}
							onPick={(connector) => write({ capture_monitor: connector })}
							onRelease={doRelease}
							// The host says which backends honour a keep-list.
							onKeepLit={
								acts("display", "keep_monitors")
									? (connector, keep) => {
											const current = policy?.keep_monitors ?? [];
											write({
												keep_monitors: keep
													? [...current, connector]
													: current.filter(
															(c) =>
																c.toLowerCase() !== connector.toLowerCase(),
														),
											});
										}
									: undefined
							}
						/>

						{/* The page's central question, answered in the open. Under the map on purpose:
						    hovering a preset redraws the map and its ghost. */}
						{policy && effective && settings.data && (
							<Card>
								<CardContent className="space-y-4">
									<CardTitle>
										<h2>{m.display_when_connects()}</h2>
									</CardTitle>
									{/* The policy in effect, never the hovered preview: text that followed the hover
									    would move the presets under the cursor. */}
									<p className="text-sm">
										{describePolicy(effective, {
											live,
											mirror: pinned,
											gameSession: policy.game_session,
										})}
										{effective.layout.mode === "manual" && (
											<> {m.display_arranged_by_you()}</>
										)}
									</p>
									<BehaviourPicker
										policy={policy}
										presets={settings.data.presets}
										customPresets={settings.data.custom_presets}
										onApply={(p) => write(p)}
										onCustomise={() => setCustomiseOpen(true)}
										onSavePreset={savePreset}
										onRenamePreset={renamePreset}
										onUpdatePreset={(p) =>
											updatePreset.mutate(
												{
													id: p.id,
													data: {
														name: p.name,
														fields: effective,
														game_session: policy.game_session ?? "auto",
													},
												},
												{ onSuccess: invalidate },
											)
										}
										onDeletePreset={removePreset}
										busy={busy}
										onPreview={setPreview}
									/>
								</CardContent>
							</Card>
						)}

						<AdvancedDisclosure
							policy={policy}
							effective={effective}
							busy={busy}
							onSet={write}
							onSetField={writeField}
						/>
						<OtherDesktops />
					</Stagger>
				</QueryState>
			</div>

			{policy && effective && (
				<CustomiseDialog
					open={customiseOpen}
					onOpenChange={setCustomiseOpen}
					effective={effective}
					policy={policy}
					enforced={settings.data?.enforced ?? []}
					onSetField={write}
					busy={busy}
				/>
			)}
		</Section>
	);
};
