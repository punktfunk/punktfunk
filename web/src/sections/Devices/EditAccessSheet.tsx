import { TimerOff, Trash2 } from "lucide-react";
import { type FC, useEffect, useState } from "react";
import type { UpdateNativeAccess } from "@/api/gen/model/updateNativeAccess";
import { Button } from "@/components/ui/button";
import {
	Dialog,
	DialogContent,
	DialogDescription,
	DialogFooter,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { m } from "@/paraglide/messages";
import {
	AccessControls,
	type AccessDraft,
	draftExpirySecs,
	draftUntilDisconnect,
	GRANT_ALL,
} from "./access";

/** What the edit sheet needs to know about the row it edits — a SNAPSHOT taken on open. */
export interface EditAccessTarget {
	fingerprint: string;
	name: string;
	grants: number | null | undefined;
	expiresUnix: number | null | undefined;
	/** The record ends with the device's last session rather than at a clock time. */
	untilDisconnect?: boolean | null;
}

/**
 * The per-device access editor (native rows only — the GameStream plane is not governed yet and
 * its rows say so instead of pretending). Preset + Advanced toggles, extend / expire now / make
 * permanent (= Expires: Forever), and remove.
 *
 * The PATCH is PARTIAL: only what changed is sent — an untouched mask stays omitted, and the
 * default "Keep current expiry" maps to omitting both expiry fields.
 */
export const EditAccessSheet: FC<{
	/** The row being edited, or null when the sheet is closed. */
	target: EditAccessTarget | null;
	nowUnix: number;
	onCancel: () => void;
	onSave: (fingerprint: string, body: UpdateNativeAccess) => void;
	/** "Expire now" — cuts live sessions from this device with the typed close. */
	onExpireNow: (fingerprint: string) => void;
	/** Hands off to the existing unpair confirmation. */
	onRemove: (fingerprint: string) => void;
	isPending: boolean;
}> = ({
	target,
	nowUnix,
	onCancel,
	onSave,
	onExpireNow,
	onRemove,
	isPending,
}) => {
	const [draft, setDraft] = useState<AccessDraft>({
		grants: GRANT_ALL,
		expiry: "keep",
		customHours: 4,
	});

	// Re-arm for each newly opened row; `target` is a snapshot, so this never fires mid-edit.
	useEffect(() => {
		if (!target) return;
		setDraft({
			grants: (target.grants ?? GRANT_ALL) & GRANT_ALL,
			// A session grant opens as one: it has no deadline, so "Forever" would both misread
			// the row and, on save, quietly convert it to permanent.
			// A permanent device has no expiry to keep — "Forever" is its no-change state.
			expiry: target.untilDisconnect
				? "session"
				: target.expiresUnix != null
					? "keep"
					: "forever",
			customHours: 4,
		});
	}, [target]);

	const expired = target?.expiresUnix != null && target.expiresUnix <= nowUnix;

	const save = () => {
		if (!target) return;
		const body: UpdateNativeAccess = {};
		if (draft.grants !== ((target.grants ?? GRANT_ALL) & GRANT_ALL))
			body.grants = draft.grants;
		if (draft.expiry === "forever") {
			// Only an actual change clears — `clear_expiry` on a permanent device is a no-op
			// request the host doesn't need to see.
			if (target.expiresUnix != null) body.clear_expiry = true;
		} else {
			const secs = draftExpirySecs(draft);
			if (secs != null) body.expires_in_secs = secs;
		}
		// Only when it moves: an omitted field keeps whatever the record already says.
		const session = draftUntilDisconnect(draft);
		if (session !== (target.untilDisconnect ?? false))
			body.until_disconnect = session;
		// Moving to "this session" has to clear any deadline the row still carries. The host
		// honours whichever lands first, but the chip shows only the session half — so a
		// surviving deadline would cut the guest off with nothing on screen explaining it.
		if (session && target.expiresUnix != null) body.clear_expiry = true;
		if (Object.keys(body).length === 0) {
			onCancel(); // nothing changed — no request to make
			return;
		}
		onSave(target.fingerprint, body);
	};

	return (
		<Dialog open={target !== null} onOpenChange={(open) => !open && onCancel()}>
			{target && (
				<DialogContent className="max-w-md">
					<DialogHeader>
						<DialogTitle>{m.access_edit_title()}</DialogTitle>
						<DialogDescription>
							{m.access_edit_desc({
								name: target.name || target.fingerprint.slice(0, 16),
							})}
						</DialogDescription>
					</DialogHeader>

					<AccessControls
						value={draft}
						onChange={setDraft}
						idPrefix="edit-access"
						allowKeepExpiry={target.expiresUnix != null}
					/>

					<div className="flex flex-wrap gap-2">
						{/* An expired device has nothing left to cut — re-granting is the verb then. */}
						{!expired && (
							<Button
								variant="outline"
								size="sm"
								disabled={isPending}
								onClick={() => onExpireNow(target.fingerprint)}
							>
								<TimerOff className="size-4" />
								{m.access_expire_now()}
							</Button>
						)}
						<Button
							variant="destructive"
							size="sm"
							disabled={isPending}
							onClick={() => onRemove(target.fingerprint)}
						>
							<Trash2 className="size-4" />
							{m.action_unpair()}
						</Button>
					</div>

					<DialogFooter>
						<Button variant="outline" onClick={onCancel} disabled={isPending}>
							{m.common_cancel()}
						</Button>
						<Button disabled={isPending} onClick={save}>
							{m.common_save()}
						</Button>
					</DialogFooter>
				</DialogContent>
			)}
		</Dialog>
	);
};
