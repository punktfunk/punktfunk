// How this host treats a device that connects (design/web-console-structure-2026-10.md §5.4).
//
// The presets are ON THE PAGE, one line each: hiding them behind a button hid the page's answers.
// Hovering or focusing one redraws the map and the ghost; the sentence above says what is in
// force. Customise is a dialog: the questions and a live sentence, a focused edit.
//
// One persistence model: every pick saves. The host applies at the next connect either way.

import { Pencil, Plus, RefreshCw, Trash2 } from "lucide-react";
import { motion } from "motion/react";
import type { FC } from "react";
import type {
	CustomPreset,
	DisplayPolicy,
	EffectivePolicy,
} from "@/api/gen/model";
import { ROW, ROW_GAP, Stagger } from "@/components/stagger";
import { Button, type ButtonProps } from "@/components/ui/button";
import {
	Dialog,
	DialogContent,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { RowActions } from "@/components/ui/menu";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";
import { describePolicy } from "./describePolicy";
import { type PolicyPatch, PolicyQuestions } from "./PolicyQuestions";

/** Default first (the safe baseline), then the situational ones. */
const PRESET_ORDER = [
	"default",
	"shared-desktop",
	"hotdesk",
	"workstation",
	"gaming-rig",
] as const;

/** Customise's questions, in the order they are asked. The cap lives in Advanced. */
const HOST_AXES = [
	"keep_alive",
	"topology",
	"mode_conflict",
	"identity",
	"game_session",
] as const;

export interface BehaviourPickerProps {
	/** The stored policy: which preset is ringed, and the base a preset click writes on top of. */
	policy: DisplayPolicy;
	presets: { id: string; summary: string; fields: EffectivePolicy }[];
	customPresets: CustomPreset[];
	/** Apply a whole policy (a preset click). */
	onApply: (policy: DisplayPolicy) => void;
	/** Open the questions. */
	onCustomise: () => void;
	onSavePreset: () => void;
	onRenamePreset: (p: CustomPreset) => void;
	onUpdatePreset: (p: CustomPreset) => void;
	onDeletePreset: (p: CustomPreset) => void;
	busy?: boolean;
	/** Preview the hovered preset on the map. */
	onPreview?: (fields: EffectivePolicy | undefined) => void;
}

/** The presets, in the open: one pill each. */
export const BehaviourPicker: FC<BehaviourPickerProps> = ({
	policy,
	presets,
	customPresets,
	onApply,
	onCustomise,
	onSavePreset,
	onRenamePreset,
	onUpdatePreset,
	onDeletePreset,
	busy,
	onPreview,
}) => {
	const current = policy.preset ?? "custom";
	// Hover and focus both preview: a keyboard user gets the answer a mouse user does.
	const preview = (fields?: EffectivePolicy) => ({
		onMouseEnter: () => onPreview?.(fields),
		onMouseLeave: () => onPreview?.(undefined),
		onFocus: () => onPreview?.(fields),
		onBlur: () => onPreview?.(undefined),
	});
	return (
		<Stagger gap={ROW_GAP} className="flex flex-wrap items-center gap-2">
			{PRESET_ORDER.map((id) => {
				const p = presets.find((x) => x.id === id);
				if (!p) return null;
				return (
					<Pill
						key={id}
						selected={current === id}
						busy={busy}
						title={describePolicy(p.fields)}
						onClick={() => onApply({ ...policy, preset: id })}
						{...preview(p.fields)}
					>
						{presetLabel(id)}
					</Pill>
				);
			})}
			{customPresets.map((p) => (
				<span key={p.id} className="flex items-center">
					<Pill
						selected={false}
						busy={busy}
						title={describePolicy(p.fields)}
						onClick={() =>
							onApply({
								...policy,
								preset: "custom",
								...p.fields,
								game_session: p.game_session ?? policy.game_session ?? "auto",
							})
						}
						{...preview(p.fields)}
					>
						{p.name}
					</Pill>
					<RowActions
						disabled={busy}
						actions={[
							{
								label: m.display_preset_edit(),
								icon: <Pencil />,
								iconOnly: true,
								onSelect: () => onRenamePreset(p),
							},
							{
								label: m.display_preset_update(),
								icon: <RefreshCw />,
								iconOnly: true,
								onSelect: () => onUpdatePreset(p),
							},
							{
								label: m.display_preset_delete(),
								icon: <Trash2 />,
								iconOnly: true,
								destructive: true,
								onSelect: () => onDeletePreset(p),
							},
						]}
					/>
				</span>
			))}
			<Pill
				selected={current === "custom"}
				busy={busy}
				title={m.display_customise_desc()}
				onClick={onCustomise}
			>
				{m.display_customise()}
			</Pill>
			<motion.span variants={ROW} className="ml-auto">
				<Button
					variant="ghost"
					size="sm"
					disabled={busy}
					onClick={onSavePreset}
				>
					<Plus className="size-4" />
					{m.display_preset_save_as()}
				</Button>
			</motion.span>
		</Stagger>
	);
};

/** One preset; the pills come in one after another, like the cards they replaced. */
const Pill: FC<ButtonProps & { selected: boolean; busy?: boolean }> = ({
	selected,
	busy,
	className,
	children,
	...props
}) => (
	<motion.span variants={ROW} className="inline-flex">
		<Button
			size="sm"
			variant="outline"
			aria-pressed={selected}
			disabled={busy}
			className={cn(selected && "ring-2 ring-primary", className)}
			{...props}
		>
			{children}
		</Button>
	</motion.span>
);

/** The questions, in the one place on this page that asks for attention. */
export const CustomiseDialog: FC<{
	open: boolean;
	onOpenChange: (open: boolean) => void;
	effective: EffectivePolicy;
	policy: DisplayPolicy;
	/** The axes this host acts on. */
	enforced: readonly string[];
	onSetField: (patch: Partial<DisplayPolicy>) => void;
	busy?: boolean;
}> = ({
	open,
	onOpenChange,
	effective,
	policy,
	enforced,
	onSetField,
	busy,
}) => {
	// A preset field switches the policy to Custom, pinning the effective values: the host would
	// otherwise fill unset fields with ITS defaults and one save would change several things.
	// The game-session choice rides beside any preset and switches nothing.
	const set = (patch: PolicyPatch) => {
		const { game_session, ...fields } = patch as Partial<DisplayPolicy>;
		if (game_session != null) return onSetField({ game_session });
		onSetField(
			policy.preset === "custom"
				? fields
				: { preset: "custom", ...effective, ...fields },
		);
	};
	return (
		<Dialog open={open} onOpenChange={onOpenChange}>
			<DialogContent className="max-h-[85vh] overflow-y-auto sm:max-w-2xl">
				<DialogHeader>
					<DialogTitle>{m.display_customise()}</DialogTitle>
				</DialogHeader>
				<PolicyQuestions
					value={{ ...effective, game_session: policy.game_session ?? "auto" }}
					axes={HOST_AXES.filter((a) => enforced.includes(a))}
					onSet={set}
					busy={busy}
					// Updates on every change: a mid-edit policy cannot touch a live session, and
					// the answer is on screen before the next connect.
					footer={
						<p className="rounded-md border bg-muted/40 p-3 text-sm">
							{describePolicy(effective, { gameSession: policy.game_session })}
						</p>
					}
				/>
				<div className="flex justify-end border-t pt-4">
					<Button variant="outline" onClick={() => onOpenChange(false)}>
						{m.common_done()}
					</Button>
				</div>
			</DialogContent>
		</Dialog>
	);
};

export const presetLabel = (id: string): string =>
	({
		default: m.display_preset_default(),
		"gaming-rig": m.display_preset_gaming_rig(),
		"shared-desktop": m.display_preset_shared_desktop(),
		hotdesk: m.display_preset_hotdesk(),
		workstation: m.display_preset_workstation(),
	})[id] ?? id;
