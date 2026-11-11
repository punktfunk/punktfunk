import {
	createContext,
	type FC,
	type ReactNode,
	useContext,
	useMemo,
	useState,
} from "react";
import { Button } from "@/components/ui/button";
import {
	Dialog,
	DialogContent,
	DialogDescription,
	DialogFooter,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { isImeEvent } from "@/lib/ime";
import { m } from "@/paraglide/messages";

/**
 * `window.confirm` / `window.prompt`, replaced by the console's own Dialog.
 *
 * WHY A PROVIDER AND NOT A COMPONENT PER SITE. The native calls this replaces are *expressions* —
 * `if (!confirm(…)) return;` — sixteen of them, threaded through mutation handlers and one router
 * navigation guard. Rewriting each into "hold the pending action in state, render a dialog, run the
 * action from its onConfirm" would have put a piece of dialog machinery in every section file and
 * turned each linear handler inside out. Handing back a PROMISE keeps the call sites the shape they
 * already are:
 *
 *     if (!(await confirm({ title: … }))) return;
 *
 * which is also why the navigation guard could come along: TanStack's `shouldBlockFn` accepts
 * `Promise<boolean>`. The one native prompt that necessarily stays is `beforeunload` — a reload or
 * a tab close is the browser's dialog to draw, not ours.
 */
export type ConfirmOptions = {
	/** The question. Short — it is a heading. */
	title: string;
	/** What the operator is not being told by the title alone: the consequence. */
	description?: string;
	/** Affirmative label. Use the same verb as the control that opened the dialog. */
	confirmLabel?: string;
	/** Paint the affirmative button red — anything that destroys, removes, or interrupts. */
	destructive?: boolean;
};

export type PromptOptions = {
	title: string;
	description?: string;
	confirmLabel?: string;
	/** Field label. */
	label: string;
	defaultValue?: string;
	placeholder?: string;
};

type Dialogs = {
	/** Resolves true only if the operator confirmed; false on cancel, Esc, or overlay click. */
	confirm: (options: ConfirmOptions) => Promise<boolean>;
	/** Resolves the entered text, or null if the operator backed out — same contract as `prompt`. */
	promptText: (options: PromptOptions) => Promise<string | null>;
};

type Pending =
	| { kind: "confirm"; options: ConfirmOptions; settle: (ok: boolean) => void }
	| {
			kind: "prompt";
			options: PromptOptions;
			settle: (text: string | null) => void;
	  };

const DialogsContext = createContext<Dialogs | null>(null);

/** Settle a request negatively. Must switch on `kind` — the two `settle`s take different types. */
const cancel = (p: Pending | null) => {
	if (!p) return;
	if (p.kind === "confirm") p.settle(false);
	else p.settle(null);
};

export const DialogsProvider: FC<{ children: ReactNode }> = ({ children }) => {
	const [pending, setPending] = useState<Pending | null>(null);
	const [draft, setDraft] = useState("");

	// `setPending` takes the updater form so a second ask while one is already open cannot strand the
	// first promise unresolved — the caller would await forever. Two at once shouldn't happen (the
	// open dialog is modal), but the navigation guard can fire from outside the page's own UI.
	const api = useMemo<Dialogs>(
		() => ({
			confirm: (options) =>
				new Promise<boolean>((resolve) =>
					setPending((prev) => {
						cancel(prev);
						return { kind: "confirm", options, settle: resolve };
					}),
				),
			promptText: (options) =>
				new Promise<string | null>((resolve) => {
					setDraft(options.defaultValue ?? "");
					setPending((prev) => {
						cancel(prev);
						return { kind: "prompt", options, settle: resolve };
					});
				}),
		}),
		[],
	);

	/** Resolve and close. Every exit — button, Esc, overlay — goes through here exactly once. */
	const close = (value: boolean | string | null) => {
		if (!pending) return;
		if (pending.kind === "confirm") pending.settle(value === true);
		else pending.settle(typeof value === "string" ? value : null);
		setPending(null);
	};

	const cancelled = () => close(pending?.kind === "prompt" ? null : false);
	const options = pending?.options;

	return (
		<DialogsContext.Provider value={api}>
			{children}
			<Dialog
				open={pending !== null}
				onOpenChange={(open) => {
					if (!open) cancelled();
				}}
			>
				{pending && options && (
					<DialogContent className="max-w-md">
						<DialogHeader>
							<DialogTitle>{options.title}</DialogTitle>
							{/* Radix warns when a dialog has no description; render the element only when
							    there is one to say, and tell it so explicitly otherwise. */}
							{options.description ? (
								<DialogDescription>{options.description}</DialogDescription>
							) : (
								<DialogDescription className="sr-only">
									{options.title}
								</DialogDescription>
							)}
						</DialogHeader>

						{pending.kind === "prompt" && (
							<div className="space-y-2">
								<Label htmlFor="dialog-prompt">{pending.options.label}</Label>
								<Input
									id="dialog-prompt"
									// Autofocus is right here and nowhere else: a modal text prompt exists
									// to be typed into, and it took the focus automatically as a `prompt()`.
									autoFocus
									autoComplete="off"
									value={draft}
									placeholder={pending.options.placeholder}
									onChange={(e) => setDraft(e.target.value)}
									onKeyDown={(e) => {
										// Enter submits — the reflex `prompt()` trained everyone into.
										if (e.key === "Enter" && !isImeEvent(e)) {
											e.preventDefault();
											close(draft);
										}
									}}
								/>
							</div>
						)}

						<DialogFooter>
							<Button variant="outline" onClick={cancelled}>
								{m.common_cancel()}
							</Button>
							<Button
								variant={
									pending.kind === "confirm" && pending.options.destructive
										? "destructive"
										: "default"
								}
								onClick={() => close(pending.kind === "prompt" ? draft : true)}
							>
								{options.confirmLabel ??
									(pending.kind === "prompt"
										? m.common_save()
										: m.common_confirm())}
							</Button>
						</DialogFooter>
					</DialogContent>
				)}
			</Dialog>
		</DialogsContext.Provider>
	);
};

/**
 * The console's `confirm` / `prompt`. Both return a promise, so a handler reads top to bottom:
 *
 *     const onDelete = async () => {
 *       if (!(await confirm({ title: m.x(), confirmLabel: m.y(), destructive: true }))) return;
 *       await remove.mutateAsync(…);
 *     };
 */
export const useDialogs = (): Dialogs => {
	const ctx = useContext(DialogsContext);
	if (!ctx)
		throw new Error(
			"useDialogs must be used inside <DialogsProvider> (__root)",
		);
	return ctx;
};
