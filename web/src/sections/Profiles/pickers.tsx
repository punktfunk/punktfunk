// Add profile's three pickers: the picture is the avatar itself, the colours are swatches with
// one free choice, and where the profile plays is a set of cards, one answer each.
import { Check, ImagePlus, Plus } from "lucide-react";
import { RadioGroup } from "radix-ui";
import {
	type DragEvent,
	type FC,
	type ReactNode,
	useEffect,
	useRef,
	useState,
} from "react";
import { ProfileAvatar } from "@/components/profile-avatar";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";

const PICTURE_TYPES = ["image/png", "image/jpeg"];

/** A preview URL for `file`, revoked when it changes or the picker goes away. */
function usePreview(file: File | null): string | null {
	const [url, setUrl] = useState<string | null>(null);
	useEffect(() => {
		if (!file) return setUrl(null);
		const next = URL.createObjectURL(file);
		setUrl(next);
		return () => URL.revokeObjectURL(next);
	}, [file]);
	return url;
}

/** The new profile's avatar, which is also where its picture is chosen: a click or a drop. */
export const PicturePicker: FC<{
	name: string;
	accent: string;
	picture: File | null;
	onPick: (file: File | null) => void;
}> = ({ name, accent, picture, onPick }) => {
	const input = useRef<HTMLInputElement>(null);
	const preview = usePreview(picture);
	const [over, setOver] = useState(false);
	const drop = (e: DragEvent) => {
		e.preventDefault();
		setOver(false);
		const file = e.dataTransfer.files[0];
		if (file && PICTURE_TYPES.includes(file.type)) onPick(file);
	};
	return (
		<div className="flex w-24 shrink-0 flex-col items-center gap-1.5">
			<button
				type="button"
				aria-label={m.profiles_picture_set()}
				title={m.profiles_picture_set()}
				onClick={() => input.current?.click()}
				onDragOver={(e) => {
					e.preventDefault();
					setOver(true);
				}}
				onDragLeave={() => setOver(false)}
				onDrop={drop}
				className={cn(
					"relative rounded-full outline-none ring-offset-2 ring-offset-background transition-shadow focus-visible:ring-2 focus-visible:ring-primary",
					over && "ring-2 ring-primary",
				)}
			>
				<ProfileAvatar
					profile={{
						id: "new",
						display_name: name.trim() || "?",
						accent,
						avatar: preview,
					}}
					className="size-20 text-2xl"
				/>
				<span className="absolute -right-0.5 -bottom-0.5 flex size-7 items-center justify-center rounded-full border-2 border-background bg-primary text-primary-foreground">
					<ImagePlus className="size-3.5" />
				</span>
			</button>
			{picture ? (
				<button
					type="button"
					className="text-xs text-muted-foreground hover:text-foreground"
					onClick={() => onPick(null)}
				>
					{m.profiles_picture_remove()}
				</button>
			) : (
				<span className="text-center text-xs text-muted-foreground">
					{m.profiles_picture_optional()}
				</span>
			)}
			<input
				ref={input}
				type="file"
				accept={PICTURE_TYPES.join(",")}
				className="hidden"
				onChange={(e) => {
					const file = e.target.files?.[0];
					e.target.value = "";
					if (file) onPick(file);
				}}
			/>
		</div>
	);
};

/** The preset colours, then any colour the system picker gives: the host takes every `#RRGGBB`. */
export const AccentPicker: FC<{
	colours: readonly string[];
	value: string;
	onChange: (colour: string) => void;
}> = ({ colours, value, onChange }) => {
	const custom = !colours.includes(value);
	return (
		<fieldset className="space-y-2">
			<legend className="mb-2 text-sm font-medium">
				{m.profiles_colour()}
			</legend>
			<div className="flex flex-wrap gap-1.5">
				{colours.map((c) => (
					<Swatch
						key={c}
						colour={c}
						selected={value === c}
						onClick={() => onChange(c)}
					/>
				))}
				<label
					title={m.profiles_colour_custom()}
					className={cn(
						"relative flex size-8 cursor-pointer items-center justify-center rounded-full ring-offset-2 ring-offset-background focus-within:ring-2 focus-within:ring-primary",
						custom && "ring-2",
					)}
					style={
						custom
							? { backgroundColor: value, ["--tw-ring-color" as string]: value }
							: {
									background:
										"conic-gradient(#ef4444, #eab308, #22c55e, #14b8a6, #3b82f6, #a855f7, #ec4899, #ef4444)",
								}
					}
				>
					{custom ? (
						<Check className="size-4 text-white drop-shadow" />
					) : (
						<Plus className="size-4 text-white drop-shadow" />
					)}
					<input
						type="color"
						aria-label={m.profiles_colour_custom()}
						className="sr-only"
						value={value}
						onChange={(e) => onChange(e.target.value)}
					/>
				</label>
			</div>
		</fieldset>
	);
};

const Swatch: FC<{
	colour: string;
	selected: boolean;
	onClick: () => void;
}> = ({ colour, selected, onClick }) => (
	<button
		type="button"
		aria-label={colour}
		aria-pressed={selected}
		onClick={onClick}
		className={cn(
			"flex size-8 items-center justify-center rounded-full ring-offset-2 ring-offset-background outline-none transition-transform hover:scale-110 focus-visible:ring-2 focus-visible:ring-primary",
			selected && "ring-2",
		)}
		style={{
			backgroundColor: colour,
			["--tw-ring-color" as string]: selected ? colour : undefined,
		}}
	>
		{selected && <Check className="size-4 text-white drop-shadow" />}
	</button>
);

export interface Choice<V extends string> {
	id: V;
	label: string;
	hint: string;
	icon: ReactNode;
	disabled?: boolean;
}

/** One answer from a few, each a card: its icon, its words, and a radio mark that is ours. */
export function ChoiceCards<V extends string>({
	label,
	value,
	choices,
	onChange,
}: {
	label: string;
	value: V;
	choices: Choice<V>[];
	onChange: (value: V) => void;
}) {
	return (
		<div className="space-y-2">
			<p className="text-sm font-medium">{label}</p>
			<RadioGroup.Root
				aria-label={label}
				value={value}
				onValueChange={(v) => onChange(v as V)}
				className="flex flex-col gap-2"
			>
				{choices.map((c) => (
					<RadioGroup.Item
						key={c.id}
						value={c.id}
						disabled={c.disabled}
						className="group flex w-full items-center gap-3 rounded-lg border p-3 text-left outline-none transition-colors hover:bg-primary/5 focus-visible:ring-2 focus-visible:ring-primary disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-transparent data-[state=checked]:border-primary data-[state=checked]:bg-primary/10"
					>
						<span className="flex size-9 shrink-0 items-center justify-center rounded-md bg-muted text-muted-foreground group-data-[state=checked]:bg-primary/20 group-data-[state=checked]:text-foreground [&_svg]:size-4">
							{c.icon}
						</span>
						<span className="min-w-0 flex-1">
							<span className="block text-sm font-medium">{c.label}</span>
							<span className="block text-xs text-muted-foreground">
								{c.hint}
							</span>
						</span>
						<span className="flex size-4 shrink-0 items-center justify-center rounded-full border border-input group-data-[state=checked]:border-primary">
							<RadioGroup.Indicator className="size-2 rounded-full bg-primary" />
						</span>
					</RadioGroup.Item>
				))}
			</RadioGroup.Root>
		</div>
	);
}
