// One control per setting kind.
//
// Every control writes one value and nothing else: the page saves on change, the host answers with
// the new state, and that answer is what renders next. Text keeps a local draft and commits on
// blur or Enter. A number does the same and also saves once it has been still for a moment, so
// stepping it or typing in it needs no click elsewhere. Neither sends a request per keystroke.
import { Plus, X } from "lucide-react";
import { type FC, useEffect, useState } from "react";
import type { SettingState } from "@/api/gen/model";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { InputNumber } from "@/components/ui/input-number";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { isImeEvent } from "@/lib/ime";
import { m } from "@/paraglide/messages";
import { SETTING_COPY } from "./copy";
import { saveOnPause } from "./saveOnPause";

export type ControlProps = {
	row: SettingState;
	disabled: boolean;
	onSet: (value: unknown) => void;
	/** App names playing audio on the host right now, for the voice-chat list. */
	playingApps?: string[];
};

const Segmented: FC<{
	options: { value: string; label: string }[];
	value: string;
	disabled: boolean;
	onSet: (value: string) => void;
	label: string;
}> = ({ options, value, disabled, onSet, label }) => (
	// biome-ignore lint/a11y/useSemanticElements: a group of pressed buttons, not a form fieldset.
	<div role="group" aria-label={label} className="flex flex-wrap gap-2">
		{options.map((o) => (
			<Button
				key={o.value}
				size="sm"
				variant={value === o.value ? "default" : "outline"}
				aria-pressed={value === o.value}
				disabled={disabled}
				onClick={() => value !== o.value && onSet(o.value)}
			>
				{o.label}
			</Button>
		))}
	</div>
);

const optionLabel = (id: string, option: string) =>
	SETTING_COPY[id]?.options?.[option]?.() ?? option;

export const labelOf = (row: SettingState) =>
	SETTING_COPY[row.id]?.label() ?? row.title;

const BoolControl: FC<ControlProps> = ({ row, disabled, onSet }) => (
	<Segmented
		label={labelOf(row)}
		options={[
			{ value: "off", label: m.common_off() },
			{ value: "on", label: m.common_on() },
		]}
		value={row.value === true ? "on" : "off"}
		disabled={disabled}
		onSet={(v) => onSet(v === "on")}
	/>
);

const EnumControl: FC<ControlProps> = ({ row, disabled, onSet }) => {
	const options = (row.options ?? []).map((o) => ({
		value: o,
		label: optionLabel(row.id, o),
	}));
	const value = String(row.value);
	if (options.length <= 4) {
		return (
			<Segmented
				label={labelOf(row)}
				options={options}
				value={value}
				disabled={disabled}
				onSet={onSet}
			/>
		);
	}
	return (
		<Select value={value} onValueChange={onSet} disabled={disabled}>
			<SelectTrigger className="w-56" aria-label={labelOf(row)}>
				<SelectValue />
			</SelectTrigger>
			<SelectContent>
				{options.map((o) => (
					<SelectItem key={o.value} value={o.value}>
						{o.label}
					</SelectItem>
				))}
			</SelectContent>
		</Select>
	);
};

// Longer than the gap between two keystrokes or spinner clicks, short enough to save before a reload.
const SAVE_AFTER_MS = 600;

// `InputNumber` hands `onChange` only finite in-range numbers, and a blur clamps through it too.
// Blur, Enter and unmount save a waiting edit at once; the host's answer never replaces one.
const NumberControl: FC<ControlProps> = ({ row, disabled, onSet }) => {
	const committed = Number(row.value);
	const [draft, setDraft] = useState(committed);
	const [saver] = useState(() => saveOnPause<number>(SAVE_AFTER_MS, committed));
	useEffect(() => {
		if (saver.follow(committed)) setDraft(committed);
	}, [saver, committed]);
	// A refused write leaves the host's value as it was: show it again once the row is free.
	useEffect(() => {
		if (!disabled && saver.follow(committed)) setDraft(committed);
	}, [saver, committed, disabled]);
	useEffect(() => saver.flush, [saver]);
	const decimal = row.kind === "decimal";
	return (
		<div className="flex items-center gap-2">
			<InputNumber
				className="w-24"
				aria-label={labelOf(row)}
				value={draft}
				min={row.min ?? undefined}
				max={row.max ?? undefined}
				step={decimal ? 0.1 : 1}
				inputMode={decimal ? "decimal" : "numeric"}
				disabled={disabled}
				onChange={(n) => {
					setDraft(n);
					saver.change(n, onSet);
				}}
				onBlur={saver.flush}
				onKeyDown={(e) =>
					e.key === "Enter" && !isImeEvent(e) && e.currentTarget.blur()
				}
			/>
			{row.unit && (
				<span className="text-sm text-muted-foreground">{row.unit}</span>
			)}
		</div>
	);
};

const TextControl: FC<ControlProps> = ({ row, disabled, onSet }) => {
	const committed = String(row.value ?? "");
	const [draft, setDraft] = useState(committed);
	useEffect(() => setDraft(committed), [committed]);
	return (
		<Input
			className="w-56"
			aria-label={labelOf(row)}
			value={draft}
			maxLength={row.max_len ?? undefined}
			disabled={disabled}
			onChange={(e) => setDraft(e.target.value)}
			onBlur={() => draft.trim() !== committed && onSet(draft.trim())}
			onKeyDown={(e) =>
				e.key === "Enter" && !isImeEvent(e) && e.currentTarget.blur()
			}
		/>
	);
};

const ListControl: FC<ControlProps> = ({
	row,
	disabled,
	onSet,
	playingApps,
}) => {
	const items = Array.isArray(row.value) ? (row.value as string[]) : [];
	const [draft, setDraft] = useState("");
	const add = (name: string) => {
		const clean = name.trim().toLowerCase();
		if (!clean || clean.includes(",") || items.includes(clean)) return;
		onSet([...items, clean]);
		setDraft("");
	};
	const suggestions = (playingApps ?? []).filter(
		(a) => !items.includes(a.toLowerCase()),
	);
	return (
		<div className="flex max-w-md flex-col gap-2">
			<ul className="flex flex-wrap gap-1.5">
				{items.map((it) => (
					<li
						key={it}
						className="inline-flex items-center gap-1 rounded-md border px-2 py-0.5 text-sm"
					>
						{it}
						<button
							type="button"
							className="text-muted-foreground hover:text-foreground disabled:opacity-50"
							aria-label={`${m.common_remove()} ${it}`}
							disabled={disabled}
							onClick={() => onSet(items.filter((x) => x !== it))}
						>
							<X className="size-3" aria-hidden />
						</button>
					</li>
				))}
			</ul>
			{suggestions.length > 0 && (
				<div className="flex flex-wrap items-center gap-1.5 text-xs text-muted-foreground">
					{m.host_settings_playing_now()}
					{suggestions.map((a) => (
						<Button
							key={a}
							size="sm"
							variant="outline"
							className="h-7"
							disabled={disabled}
							onClick={() => add(a)}
						>
							<Plus className="size-3" aria-hidden />
							{a}
						</Button>
					))}
				</div>
			)}
			<form
				className="flex gap-2"
				onSubmit={(e) => {
					e.preventDefault();
					add(draft);
				}}
			>
				<Input
					className="w-48"
					aria-label={labelOf(row)}
					value={draft}
					disabled={disabled}
					onChange={(e) => setDraft(e.target.value)}
				/>
				<Button
					type="submit"
					size="input"
					variant="outline"
					disabled={disabled || !draft.trim()}
				>
					{m.host_settings_add()}
				</Button>
			</form>
		</div>
	);
};

const BY_KIND: Record<SettingState["kind"], FC<ControlProps>> = {
	bool: BoolControl,
	int: NumberControl,
	decimal: NumberControl,
	enum: EnumControl,
	text: TextControl,
	list: ListControl,
};

export const Control: FC<ControlProps> = (props) => {
	const C = BY_KIND[props.row.kind];
	return <C {...props} />;
};
