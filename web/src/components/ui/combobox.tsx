// A text field that suggests, in the console's own chrome.
//
// Shaped like the rest of `components/ui/*`: radix behind it, this app's tokens on it. The
// browser's `<datalist>` did the same job in ten lines but rendered as an OS menu — a different
// font, a different highlight, no artwork — sitting under a field that looked like ours.
//
// It is an AUTOCOMPLETE, not a shadcn Combobox, and the difference is deliberate: a combobox
// yields only values from its list, and these fields have to stay free text. A hook may name a
// game that is not installed yet or a device that has not paired, and a widget that refused to
// write those would be worse than the plain box it replaced. So the input is the value, the list
// only offers — which is `aria-autocomplete="list"`, the role that actually describes it.
//
// Scale lives here too. A library runs to five figures, so matches are filtered and capped before
// they reach the DOM; artwork is lazy and only the shown rows request it.
import { Popover } from "radix-ui";
import {
	type ComponentProps,
	type ReactNode,
	useEffect,
	useId,
	useMemo,
	useRef,
	useState,
} from "react";
import { Input } from "@/components/ui/input";
import { isImeEvent } from "@/lib/ime";
import { cn } from "@/lib/utils";

export interface ComboboxOption {
	/** What lands in the field — an app id, a device name. */
	value: string;
	/** The line a person reads. Falls back to the value. */
	label?: string;
	/** Cover art, when the thing has a face. Stepped down to `fallback` if it fails to load. */
	image?: string | null;
	/** Shown in place of artwork: a launcher mark, a monogram, nothing. */
	fallback?: ReactNode;
}

export function Combobox({
	value,
	onChange,
	options,
	max = 50,
	empty,
	className,
	...props
}: {
	value: string;
	onChange: (value: string) => void;
	options: ComboboxOption[];
	/** How many rows reach the DOM. */
	max?: number;
	/** Said when nothing matches — the field still accepts what was typed. */
	empty?: string;
} & Omit<ComponentProps<typeof Input>, "value" | "onChange" | "type">) {
	const listId = useId();
	const [open, setOpen] = useState(false);
	const [active, setActive] = useState(0);
	const inputRef = useRef<HTMLInputElement>(null);
	const listRef = useRef<HTMLDivElement>(null);

	const shown = useMemo(() => {
		const needle = value.trim().toLowerCase();
		const out: ComboboxOption[] = [];
		for (const o of options) {
			if (
				!needle ||
				o.value.toLowerCase().includes(needle) ||
				o.label?.toLowerCase().includes(needle)
			) {
				out.push(o);
				if (out.length >= max) break;
			}
		}
		return out;
	}, [options, value, max]);

	// A filter that shortens the list must not leave the highlight past its end.
	useEffect(() => {
		setActive((a) => (a >= shown.length ? 0 : a));
	}, [shown.length]);

	// Keep the highlighted row in view when the keyboard is doing the moving.
	useEffect(() => {
		if (!open) return;
		listRef.current
			?.querySelector(`[data-index="${active}"]`)
			?.scrollIntoView({ block: "nearest" });
	}, [active, open]);

	const commit = (option: ComboboxOption) => {
		onChange(option.value);
		setOpen(false);
		inputRef.current?.focus();
	};

	return (
		<Popover.Root open={open && shown.length > 0} onOpenChange={setOpen}>
			<Popover.Anchor asChild>
				<Input
					{...props}
					ref={inputRef}
					type="text"
					role="combobox"
					aria-expanded={open && shown.length > 0}
					aria-controls={listId}
					aria-autocomplete="list"
					aria-activedescendant={
						open && shown[active] ? `${listId}-${active}` : undefined
					}
					autoComplete="off"
					spellCheck={false}
					className={className}
					value={value}
					onChange={(e) => {
						onChange(e.target.value);
						setOpen(true);
					}}
					onFocus={() => setOpen(true)}
					onKeyDown={(e) => {
						// Candidate arrows, Enter and Escape belong to the IME while it composes.
						if (isImeEvent(e)) return;
						if (e.key === "ArrowDown" || e.key === "ArrowUp") {
							e.preventDefault();
							setOpen(true);
							setActive((a) => {
								const n = shown.length;
								if (n === 0) return 0;
								return e.key === "ArrowDown" ? (a + 1) % n : (a - 1 + n) % n;
							});
						} else if (e.key === "Enter" && open && shown[active]) {
							// Only when the list is open: Enter otherwise belongs to the form.
							e.preventDefault();
							commit(shown[active]);
						} else if (e.key === "Escape") {
							setOpen(false);
						}
					}}
				/>
			</Popover.Anchor>
			{/* Portalled, and ABOVE the dialog's own layer. This field lives inside a Dialog, so
			    rendering the list inline gets it clipped by that dialog's `overflow-y-auto`; at the
			    usual z-50 the portal instead paints underneath the dialog, leaving only the strip
			    below its bottom edge visible — which looks exactly like a positioning bug and is
			    not one. */}
			<Popover.Portal>
				<Popover.Content
					align="start"
					sideOffset={4}
					// The list is a suggestion, never a focus trap: the caret stays in the field
					// while it is open, so typing keeps filtering.
					onOpenAutoFocus={(e) => e.preventDefault()}
					// ...which makes the field itself "outside" as far as the dismissable layer is
					// concerned. Focusing it opened the list and dismissed it in the same gesture —
					// a flash on click, then nothing until a keystroke, because typing is not an
					// outside interaction. The anchor is exempt; everything else still closes it.
					onInteractOutside={(e) => {
						const target = e.target as Node | null;
						if (target && inputRef.current?.contains(target)) {
							e.preventDefault();
						}
					}}
					className={cn(
						"z-[100] max-h-72 w-[var(--radix-popover-trigger-width)] overflow-hidden rounded-md border bg-popover p-1 text-popover-foreground shadow-md",
						"data-[state=open]:animate-in data-[state=closed]:animate-out data-[state=closed]:fade-out-0 data-[state=open]:fade-in-0 data-[state=closed]:zoom-out-95 data-[state=open]:zoom-in-95",
					)}
				>
					<div
						ref={listRef}
						id={listId}
						role="listbox"
						className="max-h-[17rem] overflow-y-auto"
					>
						{shown.length === 0 && empty ? (
							<p className="px-2 py-3 text-center text-sm text-muted-foreground">
								{empty}
							</p>
						) : (
							shown.map((o, i) => (
								<Row
									key={o.value}
									option={o}
									id={`${listId}-${i}`}
									index={i}
									selected={o.value === value}
									active={i === active}
									onPick={() => commit(o)}
									onHover={() => setActive(i)}
								/>
							))
						)}
					</div>
				</Popover.Content>
			</Popover.Portal>
		</Popover.Root>
	);
}

function Row({
	option,
	id,
	index,
	selected,
	active,
	onPick,
	onHover,
}: {
	option: ComboboxOption;
	id: string;
	index: number;
	selected: boolean;
	active: boolean;
	onPick: () => void;
	onHover: () => void;
}) {
	const [broken, setBroken] = useState(false);
	const art = !broken && option.image ? option.image : null;
	const label = option.label ?? option.value;
	return (
		<div
			id={id}
			role="option"
			aria-selected={selected}
			// Focusable, never focused: the caret stays in the input so typing keeps filtering,
			// and the active row is announced through `aria-activedescendant` instead. -1 keeps
			// it out of the tab order while still satisfying "an interactive role is focusable".
			tabIndex={-1}
			data-index={index}
			className={cn(
				"flex cursor-pointer items-center gap-3 rounded-sm px-2 py-1.5 text-sm",
				active && "bg-accent/30",
			)}
			// `onMouseDown`, not `onClick`: a click would blur the input first, close the popover
			// on blur, and land on nothing.
			onMouseDown={(e) => {
				e.preventDefault();
				onPick();
			}}
			onMouseMove={onHover}
		>
			{(art || option.fallback) && (
				<span className="flex h-10 w-7 shrink-0 items-center justify-center overflow-hidden rounded-sm bg-muted">
					{art ? (
						<img
							src={art}
							alt=""
							loading="lazy"
							className="size-full object-cover"
							onError={() => setBroken(true)}
						/>
					) : (
						option.fallback
					)}
				</span>
			)}
			<span className="min-w-0 flex-1">
				<span className="block truncate">{label}</span>
				{option.label && option.label !== option.value && (
					<span className="block truncate font-mono text-xs text-muted-foreground">
						{option.value}
					</span>
				)}
			</span>
		</div>
	);
}
