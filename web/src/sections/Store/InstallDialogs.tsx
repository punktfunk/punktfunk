import { BadgeCheck, ShieldAlert, ShieldQuestion } from "lucide-react";
import { type FC, useEffect, useState } from "react";
import type { CatalogEntry } from "@/api/gen/model";
import type { PendingUpdate } from "@/api/store";
import { PasswordConfirmDialog } from "@/components/password-confirm";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
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
import { m } from "@/paraglide/messages";

// Friction proportional to trust. The three install paths deliberately do NOT share a confirm
// button: a verified install is one click on a plain dialog, an external one makes you read that
// unom didn't review the code and names who did curate it, and a raw package spec makes you retype
// the spec and tick a box. The last one is never reachable by accident — no card, no button on a
// listing, only the small footer link on Browse.

/**
 * Tiers 1 and 2: install a curated catalog entry. Verified entries get an ordinary confirm; an
 * entry from an operator-added source gets the warning treatment and names its source, because
 * "pinned and integrity-checked" is not the same claim as "reviewed".
 */
export const InstallDialog: FC<{
	/** The entry being confirmed, or null when the dialog is closed. */
	entry: CatalogEntry | null;
	onCancel: () => void;
	onConfirm: (entry: CatalogEntry) => void;
	isPending: boolean;
}> = ({ entry, onCancel, onConfirm, isPending }) => {
	const external = entry?.tier === "external";
	return (
		<Dialog open={entry !== null} onOpenChange={(open) => !open && onCancel()}>
			{entry && (
				<DialogContent>
					<DialogHeader>
						<DialogTitle className="flex items-center gap-2">
							{external ? (
								<ShieldQuestion className="size-5 shrink-0 text-amber-600 dark:text-amber-500" />
							) : (
								<BadgeCheck className="size-5 shrink-0 text-[var(--success)]" />
							)}
							{external
								? m.store_install_external_title({ title: entry.title })
								: m.store_install_title({ title: entry.title })}
						</DialogTitle>
						<DialogDescription>
							{external
								? m.store_install_external_body({
										version: entry.version,
										source: entry.source,
									})
								: m.store_install_verified_body({ version: entry.version })}
						</DialogDescription>
					</DialogHeader>

					<p className="rounded-md bg-muted px-3 py-2 font-mono text-xs text-muted-foreground">
						{entry.pkg}@{entry.version}
					</p>

					{external && (
						<p className="rounded-md border border-amber-600/40 bg-amber-500/10 px-3 py-2 text-sm text-amber-600 dark:border-amber-500/40 dark:text-amber-500">
							{m.store_install_external_note()}
						</p>
					)}

					<DialogFooter>
						<Button variant="outline" onClick={onCancel} disabled={isPending}>
							{m.common_cancel()}
						</Button>
						{/* Red is reserved for the raw-spec install; an external one escalates through
						    its copy and its amber panel, not by borrowing the danger colour. */}
						<Button disabled={isPending} onClick={() => onConfirm(entry)}>
							{external
								? m.store_install_external_confirm()
								: m.store_install_confirm()}
						</Button>
					</DialogFooter>
				</DialogContent>
			)}
		</Dialog>
	);
};

/**
 * "Update all": one confirmation for a whole run of catalog installs.
 *
 * It is the same trust decision as `InstallDialog`, taken once for several packages, so it keeps
 * the same escalation rule — if ANY entry in the run comes from an operator-added source, the whole
 * dialog wears the external treatment and names those sources. A bulk action must not be a way to
 * wave through, in one click, a warning each package would have shown on its own.
 */
export const UpdateAllDialog: FC<{
	/** The updates to run, in order — null when the dialog is closed. */
	updates: PendingUpdate[] | null;
	/** Plugins with an update the run will not attempt; named so the count adds up on screen. */
	skipped: string[];
	onCancel: () => void;
	onConfirm: (updates: PendingUpdate[]) => void;
	isPending: boolean;
}> = ({ updates, skipped, onCancel, onConfirm, isPending }) => {
	const external = (updates ?? []).filter((u) => u.entry.tier === "external");
	// Each source named once, in the order the run meets it.
	const sources = [...new Set(external.map((u) => u.entry.source))];
	return (
		<Dialog
			open={updates !== null}
			onOpenChange={(open) => !open && onCancel()}
		>
			{updates && (
				<DialogContent className="max-w-lg">
					<DialogHeader>
						<DialogTitle className="flex items-center gap-2">
							{sources.length > 0 ? (
								<ShieldQuestion className="size-5 shrink-0 text-amber-600 dark:text-amber-500" />
							) : (
								<BadgeCheck className="size-5 shrink-0 text-[var(--success)]" />
							)}
							{m.store_update_all_title()}
						</DialogTitle>
						<DialogDescription>{m.store_update_all_body()}</DialogDescription>
					</DialogHeader>

					{/* Every version change, spelled out: a bulk confirm that only says "3 plugins" is
					    asking the operator to trust a number. Scrolls rather than growing past the
					    dialog's own max-height when a host has a lot installed. */}
					<ul className="max-h-64 space-y-1 overflow-y-auto rounded-md bg-muted p-3 text-xs">
						{updates.map((u) => (
							<li
								key={u.plugin.pkg}
								className="flex items-baseline justify-between gap-3"
							>
								<span className="truncate font-medium">
									{u.plugin.title ?? u.plugin.pkg}
								</span>
								<span className="shrink-0 font-mono tabular-nums text-muted-foreground">
									{u.plugin.version ? `v${u.plugin.version}` : "—"} → v
									{u.entry.version}
								</span>
							</li>
						))}
					</ul>

					{sources.length > 0 && (
						<p className="rounded-md border border-amber-600/40 bg-amber-500/10 px-3 py-2 text-sm text-amber-600 dark:border-amber-500/40 dark:text-amber-500">
							{m.store_update_all_external_note({
								sources: sources.join(", "),
							})}
						</p>
					)}

					{skipped.length > 0 && (
						<p className="text-xs text-muted-foreground">
							{m.store_update_all_skipped({ names: skipped.join(", ") })}
						</p>
					)}

					<DialogFooter>
						<Button variant="outline" onClick={onCancel} disabled={isPending}>
							{m.common_cancel()}
						</Button>
						<Button disabled={isPending} onClick={() => onConfirm(updates)}>
							{sources.length > 0
								? m.store_update_all_external_confirm()
								: m.store_update_all_confirm()}
						</Button>
					</DialogFooter>
				</DialogContent>
			)}
		</Dialog>
	);
};

/**
 * Tier 3: install a raw package spec. No catalog, no review, no pinning — so the dialog spells out
 * exactly what that means and asks for two independent confirmations (retype the spec, tick the
 * box) before it will even enable its button. A refused password keeps everything typed in place.
 */
export const SpecInstallDialog: FC<{
	open: boolean;
	onClose: () => void;
	onConfirm: (spec: string, password: string) => Promise<void>;
}> = ({ open, onClose, onConfirm }) => {
	const [spec, setSpec] = useState("");
	const [echo, setEcho] = useState("");
	const [accepted, setAccepted] = useState(false);

	// Every confirmation is cleared on exit, cancel and confirm alike: reopening this dialog must
	// never find it pre-armed with the last spec or a ticked box. The dialog stays mounted.
	useEffect(() => {
		if (open) return;
		setSpec("");
		setEcho("");
		setAccepted(false);
	}, [open]);

	const wanted = spec.trim();
	return (
		<PasswordConfirmDialog
			open={open}
			id="store-spec-password"
			className="max-w-xl"
			title={
				<>
					<ShieldAlert className="size-5 shrink-0 text-destructive" />
					{m.store_spec_title()}
				</>
			}
			body={m.store_spec_lead()}
			submitLabel={m.store_spec_confirm()}
			destructive
			help={m.store_spec_password_help()}
			// The retyped spec matches exactly and the box is ticked; the password is the third gate.
			ready={wanted.length > 0 && echo.trim() === wanted && accepted}
			autoFocus={false}
			onSubmit={(password) => onConfirm(wanted, password)}
			onClose={onClose}
		>
			<p className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive">
				{m.store_spec_permanent()}
			</p>

			<div className="space-y-2">
				<Label htmlFor="store-spec">{m.store_spec_field()}</Label>
				<Input
					id="store-spec"
					autoFocus
					autoComplete="off"
					spellCheck={false}
					placeholder="@scope/plugin-name@1.2.3"
					value={spec}
					onChange={(e) => setSpec(e.target.value)}
				/>
				<p className="text-xs text-muted-foreground">
					{m.store_spec_field_help()}
				</p>
			</div>

			<div className="space-y-2">
				<Label htmlFor="store-spec-echo">{m.store_spec_confirm_field()}</Label>
				<Input
					id="store-spec-echo"
					autoComplete="off"
					spellCheck={false}
					value={echo}
					onChange={(e) => setEcho(e.target.value)}
				/>
			</div>

			<Label className="flex items-start gap-3 text-sm font-normal">
				<Checkbox
					checked={accepted}
					onCheckedChange={(next) => setAccepted(next === true)}
					className="mt-0.5"
				/>
				<span>{m.store_spec_checkbox()}</span>
			</Label>
		</PasswordConfirmDialog>
	);
};
