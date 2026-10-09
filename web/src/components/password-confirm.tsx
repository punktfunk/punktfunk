// The console-password field every gated action shows, the dialog most of them show it in, and the
// two refusals the BFF gives it (server/util/confirm.ts): 401 when the password is wrong, 429 when
// the per-peer budget is spent.
import { type FC, type ReactNode, useCallback, useState } from "react";
import { ApiError } from "@/api/fetcher";
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
import { apiErrorMessage } from "@/lib/errors";
import { m } from "@/paraglide/messages";

export type PasswordFailure = "wrong" | "throttled" | null;

/** The password refusal behind `e`, or null for any other error. */
export function passwordFailure(e: unknown): PasswordFailure {
	if (!(e instanceof ApiError)) return null;
	if (e.status === 401) return "wrong";
	if (e.status === 429) return "throttled";
	return null;
}

/**
 * The last attempt's refusal. `classify(e)` records it and returns false for any other error, so
 * the caller keeps its own failure message for those. Both callbacks are stable.
 */
export function usePasswordFailure() {
	const [failure, setFailure] = useState<PasswordFailure>(null);
	const reset = useCallback(() => setFailure(null), []);
	const classify = useCallback((e: unknown): boolean => {
		const f = passwordFailure(e);
		setFailure(f);
		return f !== null;
	}, []);
	return { failure, reset, classify };
}

export const PasswordConfirmField: FC<{
	id: string;
	value: string;
	onChange: (v: string) => void;
	failure: PasswordFailure;
	help?: ReactNode;
	autoFocus?: boolean;
	/** Out of the password manager's fill, for a page field under a dialog that asks too. */
	disabled?: boolean;
}> = ({ id, value, onChange, failure, help, autoFocus, disabled }) => (
	<div className="space-y-2">
		<Label htmlFor={id}>{m.store_spec_password()}</Label>
		{/* `name` matches sign-in, so the saved console login maps onto this field. */}
		<Input
			id={id}
			name="password"
			type="password"
			autoComplete="current-password"
			autoFocus={autoFocus}
			disabled={disabled}
			data-1p-ignore={disabled || undefined}
			value={value}
			onChange={(e) => onChange(e.target.value)}
		/>
		{help && <p className="text-xs text-muted-foreground">{help}</p>}
		{failure && (
			<p role="alert" className="text-xs text-destructive">
				{failure === "wrong"
					? m.update_apply_wrong_password()
					: m.update_apply_throttled()}
			</p>
		)}
	</div>
);

/**
 * A gated action's dialog: `children` above the password field, Enter submits, and every close
 * starts the next opening clean.
 *
 * `onSubmit` resolving closes the dialog. Returning a string keeps it open and shows the string
 * (an empty one shows nothing). A throw keeps it open too: a password refusal shows on the field,
 * anything else as the server's own message, else `failedText`.
 */
export const PasswordConfirmDialog: FC<{
	open: boolean;
	/** The field's id, unique on the page. */
	id: string;
	title: ReactNode;
	body: ReactNode;
	submitLabel: ReactNode;
	/** The button's label while `onSubmit` runs. */
	busyLabel?: ReactNode;
	destructive?: boolean;
	help?: ReactNode;
	/** The dialog's own gates (a retyped spec, a ticked box) pass. */
	ready?: boolean;
	/** Off when a field in `children` comes first. */
	autoFocus?: boolean;
	failedText?: string;
	className?: string;
	onSubmit: (password: string) => Promise<unknown>;
	onClose: () => void;
	children?: ReactNode;
}> = ({
	open,
	id,
	title,
	body,
	submitLabel,
	busyLabel,
	destructive,
	help,
	ready = true,
	autoFocus = true,
	failedText,
	className,
	onSubmit,
	onClose,
	children,
}) => {
	const [password, setPassword] = useState("");
	const [busy, setBusy] = useState(false);
	const [error, setError] = useState<string | null>(null);
	const refusal = usePasswordFailure();
	const canSubmit = ready && password.length > 0 && !busy;

	const close = () => {
		setPassword("");
		setError(null);
		refusal.reset();
		onClose();
	};
	const submit = async () => {
		if (!canSubmit) return;
		setBusy(true);
		setError(null);
		refusal.reset();
		try {
			const kept = await onSubmit(password);
			if (typeof kept === "string") setError(kept || null);
			else close();
		} catch (e) {
			if (refusal.classify(e)) return;
			setError(
				(e instanceof ApiError && apiErrorMessage(e)) ||
					failedText ||
					m.common_error(),
			);
		} finally {
			setBusy(false);
		}
	};

	return (
		<Dialog open={open} onOpenChange={(o) => !o && close()}>
			<DialogContent className={className}>
				<DialogHeader>
					<DialogTitle className="flex items-center gap-2">{title}</DialogTitle>
					<DialogDescription>{body}</DialogDescription>
				</DialogHeader>
				{children}
				<form
					className="space-y-3"
					onSubmit={(e) => {
						e.preventDefault();
						void submit();
					}}
				>
					<PasswordConfirmField
						id={id}
						value={password}
						onChange={setPassword}
						failure={refusal.failure}
						help={help}
						autoFocus={autoFocus}
					/>
					{error && <p className="text-sm text-destructive">{error}</p>}
					<DialogFooter>
						<Button
							type="button"
							variant="outline"
							onClick={close}
							disabled={busy}
						>
							{m.common_cancel()}
						</Button>
						<Button
							type="submit"
							variant={destructive ? "destructive" : "default"}
							disabled={!canSubmit}
						>
							{busy && busyLabel ? busyLabel : submitLabel}
						</Button>
					</DialogFooter>
				</form>
			</DialogContent>
		</Dialog>
	);
};
