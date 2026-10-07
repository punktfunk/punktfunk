import { Timer } from "lucide-react";
import { type FC, useEffect, useState } from "react";
import type { ApprovePending } from "@/api/gen/model/approvePending";
import type { PendingDevice } from "@/api/gen/model/pendingDevice";
import {
	PasswordConfirmField,
	type PasswordFailure,
} from "@/components/password-confirm";
import { ProfileAvatar } from "@/components/profile-avatar";
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
import { m } from "@/paraglide/messages";
import {
	type AccessDraft,
	AccessPicker,
	draftFromStored,
	GUEST_EXPIRES_SECS,
	grantFields,
	PRESET_CONTROLLER,
} from "./access";

/**
 * Approving a pending device — the ONE dialog of the co-play flow: name, access level, expiry.
 *
 * Defaults are Full control · Forever (D1): pairing your OWN new device is the common case, and a
 * default-limited dialog trains reflexive clicking-through. The guest case gets its own
 * affordance instead — "Approve as guest" (Controller only · 4 h) is one click here, two from the
 * pending row, and the expiry cleans up after the evening on its own.
 *
 * `device` is the row SNAPSHOT taken when the dialog opened, not the live polled row — the
 * pending list refetches every 10 s, and a fresh object identity mid-edit must not reset the
 * operator's typing.
 */
export const ApproveDialog: FC<{
	/** The device being approved, or null when the dialog is closed. */
	device: PendingDevice | null;
	onCancel: () => void;
	/** Approve, with the console password the BFF re-verifies — approving pairs a device outright,
	 * with no PIN ceremony, so a session cookie on its own must not be able to do it. */
	onApprove: (id: number, body: ApprovePending, password: string) => void;
	isPending: boolean;
	/** Why the BFF refused the last approve's password, if it did. */
	failure: PasswordFailure;
}> = ({ device, onCancel, onApprove, isPending, failure }) => {
	const [name, setName] = useState("");
	const [password, setPassword] = useState("");
	const [draft, setDraft] = useState<AccessDraft>(() =>
		draftFromStored(null, null, null),
	);

	// The knocking fingerprint may already be paired (the expired-guest re-knock) — then the
	// stored access pre-fills the dialog, so re-approval is re-granting what they had.
	const known =
		device != null &&
		(device.grants != null ||
			device.expires_unix != null ||
			device.access_level != null);

	// Re-arm the form for each newly opened device (and only then — never mid-edit, because the
	// snapshot's identity is stable for as long as the dialog is open).
	useEffect(() => {
		if (!device) return;
		setName(device.name);
		setPassword("");
		setDraft(
			draftFromStored(
				device.grants,
				device.expires_unix,
				device.granted_unix,
				device.until_disconnect,
			),
		);
	}, [device]);

	const trimmedName = (): string | null => (name.trim() ? name.trim() : null);

	const submit = () => {
		if (!device) return;
		// A device with a stored record gets the dialog's state explicitly: omitting the fields
		// would silently keep the stored access instead.
		onApprove(
			device.id,
			{ name: trimmedName(), ...grantFields(draft, known) },
			password,
		);
	};

	const approveAsGuest = () => {
		if (!device) return;
		onApprove(
			device.id,
			{
				name: trimmedName(),
				grants: PRESET_CONTROLLER,
				expires_in_secs: GUEST_EXPIRES_SECS,
			},
			password,
		);
	};

	return (
		<Dialog open={device !== null} onOpenChange={(open) => !open && onCancel()}>
			{device && (
				<DialogContent className="max-w-md">
					<DialogHeader>
						<DialogTitle>{m.pairing_pending_name_title()}</DialogTitle>
						{/* The name field below is editable text, not a statement of WHICH knock
						    this is — with two devices waiting the operator would be approving
						    whichever row they hope they clicked. The fingerprint is the only
						    thing that stays unique when two devices share a name, so the
						    description states it (security-review 2026-08-15, carried into the
						    access dialog that replaced the plain name prompt). */}
						<DialogDescription>
							{m.pairing_pending_name_desc({
								name: device.name,
								fp: `${device.fingerprint.slice(0, 16)}…`,
							})}{" "}
							{m.pairing_approve_desc()}
						</DialogDescription>
					</DialogHeader>

					{device.profile && (
						<p className="flex items-center gap-2 text-sm">
							<ProfileAvatar
								profile={device.profile}
								className="size-6 text-xs"
							/>
							{m.pairing_approve_profile({
								name: device.profile.display_name,
							})}
						</p>
					)}

					<div className="space-y-2">
						<Label htmlFor="approve-name">
							{m.pairing_pending_name_prompt()}
						</Label>
						<Input
							id="approve-name"
							autoFocus
							autoComplete="off"
							value={name}
							onChange={(e) => setName(e.target.value)}
						/>
					</div>

					{known && (
						<p className="rounded-md bg-muted px-3 py-2 text-xs text-muted-foreground">
							{m.pairing_approve_known_note()}
						</p>
					)}

					<AccessPicker value={draft} onChange={setDraft} idPrefix="approve" />

					{/* Approving pairs the device outright — no PIN — so it re-confirms the console
					    password, which the BFF verifies and strips (util/confirm.ts). Both approve
					    paths below carry it. Its own form, so 1Password fills here, not the page. */}
					<form
						onSubmit={(e) => {
							e.preventDefault();
							if (!isPending && password.length > 0) submit();
						}}
					>
						<PasswordConfirmField
							id="approve-password"
							value={password}
							onChange={setPassword}
							failure={failure}
							help={m.pairing_password_help()}
						/>
					</form>

					{/* The guest fast path — visually its own thing, deliberately not one of the footer
					    buttons: one click grants Controller only for 4 hours, no dialog fiddling. */}
					<div className="flex items-center justify-between gap-3 rounded-md border p-3">
						<p className="text-xs text-muted-foreground">
							{m.pairing_approve_guest_hint()}
						</p>
						<Button
							variant="secondary"
							size="sm"
							className="shrink-0"
							disabled={isPending || password.length === 0}
							onClick={approveAsGuest}
						>
							<Timer className="size-4" />
							{m.pairing_approve_guest()}
						</Button>
					</div>

					<DialogFooter>
						<Button variant="outline" onClick={onCancel} disabled={isPending}>
							{m.common_cancel()}
						</Button>
						<Button
							disabled={isPending || password.length === 0}
							onClick={submit}
						>
							{m.pairing_pending_approve()}
						</Button>
					</DialogFooter>
				</DialogContent>
			)}
		</Dialog>
	);
};
