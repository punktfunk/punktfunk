// How this host treats a device that connects (design/web-console-structure-2026-10.md §5.4).
//
// The presets are ON THE PAGE, one line each: hiding them behind a button hid the page's answers.
// Hovering or focusing one redraws the map and the ghost; the sentence above says what is in
// force. Customise is a dialog: the questions and a live sentence, a focused edit.
//
// One persistence model: every pick saves. The host applies at the next connect either way.

import {
	Check,
	Pencil,
	Plus,
	RefreshCw,
	SlidersHorizontal,
	Trash2,
} from "lucide-react";
import { motion } from "motion/react";
import type { FC, ReactNode } from "react";
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

/** The presets, in the open: one card each, its summary under its name. */
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
		<div className="space-y-3">
			<Stagger
				gap={ROW_GAP}
				className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3"
			>
				{PRESET_ORDER.map((id) => {
					const p = presets.find((x) => x.id === id);
					if (!p) return null;
					return (
						<PresetCard
							key={id}
							title={presetLabel(id)}
							summary={p.summary}
							selected={current === id}
							busy={busy}
							onClick={() => onApply({ ...policy, preset: id })}
							{...preview(p.fields)}
						/>
					);
				})}
				{customPresets.map((p) => (
					<PresetCard
						key={p.id}
						title={p.name}
						summary={describePolicy(p.fields)}
						selected={false}
						busy={busy}
						onClick={() =>
							onApply({
								...policy,
								preset: "custom",
								...p.fields,
								game_session: p.game_session ?? policy.game_session ?? "auto",
							})
						}
						actions={
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
						}
						{...preview(p.fields)}
					/>
				))}
				<PresetCard
					title={m.display_customise()}
					summary={m.display_customise_desc()}
					selected={current === "custom"}
					busy={busy}
					icon={<SlidersHorizontal className="size-4" />}
					onClick={onCustomise}
				/>
			</Stagger>
			<div className="flex justify-end">
				<Button
					variant="ghost"
					size="sm"
					disabled={busy}
					onClick={onSavePreset}
				>
					<Plus className="size-4" />
					{m.display_preset_save_as()}
				</Button>
			</div>
		</div>
	);
};

/**
 * One preset as a card: its name, what it does in a sentence, a tick while it is the policy. A
 * custom preset's own actions sit beside the card's button, never inside it.
 */
const PresetCard: FC<
	ButtonProps & {
		title: string;
		summary: string;
		selected: boolean;
		busy?: boolean;
		icon?: ReactNode;
		actions?: ReactNode;
	}
> = ({
	title,
	summary,
	selected,
	busy,
	icon,
	actions,
	className,
	...props
}) => (
	<motion.div variants={ROW} className="relative">
		<button
			type="button"
			aria-pressed={selected}
			disabled={busy}
			className={cn(
				"flex h-full w-full flex-col items-start gap-1 rounded-lg border p-3 text-left outline-none transition-colors hover:bg-primary/5 focus-visible:ring-2 focus-visible:ring-primary disabled:opacity-50",
				selected && "border-primary bg-primary/10",
				actions && "pr-24",
				className,
			)}
			{...props}
		>
			<span className="flex w-full items-center gap-2 pr-6 text-sm font-medium">
				{icon}
				<span className="truncate">{title}</span>
			</span>
			<span className="text-xs text-muted-foreground">{summary}</span>
		</button>
		{selected && (
			<Check className="pointer-events-none absolute top-3 right-3 size-4 text-primary" />
		)}
		{actions && (
			<div className="absolute top-1.5 right-1.5 flex">{actions}</div>
		)}
	</motion.div>
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
