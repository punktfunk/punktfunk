import { ChevronDown, ChevronRight } from "lucide-react";
import { type FC, useEffect, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Checkbox } from "@/components/ui/checkbox";
import { InputNumber } from "@/components/ui/input-number";
import { Label } from "@/components/ui/label";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { m } from "@/paraglide/messages";

/**
 * The access-control vocabulary + the ONE control family for all three grant moments — the
 * approve dialog, the arm card, and the paired-row edit sheet (design: per-client-access.md §6).
 *
 * Grant bits mirror `punktfunk-core` `quic/access.rs` (wire == store; reserved bits must be
 * zero). The preset label is DERIVED from the mask, never stored — no drift, and a mask the
 * presets don't cover honestly displays as "Custom".
 */
export const GRANT_GAMEPAD = 0x01;
export const GRANT_POINTER = 0x02;
export const GRANT_KEYBOARD = 0x04;
export const GRANT_CLIPBOARD = 0x08;
export const GRANT_MIC = 0x10;
export const GRANT_LAUNCH = 0x20;
export const GRANT_POWER = 0x40;
export const GRANT_MANAGE_GAMES = 0x80;
export const GRANT_ALL = 0xff;

/** `GRANT_ALL` before the Power bit existed (hosts ≤ 0.32.x). */
const GRANT_ALL_PRE_POWER = 0x3f;
/** `GRANT_ALL` before the Manage games bit existed. */
const GRANT_ALL_PRE_MANAGE = 0x7f;

/**
 * The legacy-full read rule (host-actions §4.3): a stored mask that is EXACTLY the pre-power
 * full mask was an explicit "Full control" from before Power existed — read it as today's
 * `GRANT_ALL`, so the chip stays "Full" and the edit sheet's toggles agree with it.
 */
export const normalizeLegacyFull = (mask: number): number =>
	mask === GRANT_ALL_PRE_POWER || mask === GRANT_ALL_PRE_MANAGE
		? GRANT_ALL
		: mask;

/** The guest preset (D2): controller only, WITHOUT launch — the owner drives what runs. */
export const PRESET_CONTROLLER = GRANT_GAMEPAD;
export const PRESET_VIEW = 0;
/** The one-click "Approve as guest" duration (D4): 4 hours. */
export const GUEST_EXPIRES_SECS = 4 * 3600;

export type AccessLevel = "full" | "controller" | "view" | "custom";

/** Preset name for a mask — derived, never stored (design §3.2). */
export const levelOfMask = (mask: number): AccessLevel => {
	switch (normalizeLegacyFull(mask) & GRANT_ALL) {
		case GRANT_ALL:
			return "full";
		case PRESET_CONTROLLER:
			return "controller";
		case PRESET_VIEW:
			return "view";
		default:
			return "custom";
	}
};

export const levelLabel = (level: AccessLevel): string => {
	switch (level) {
		case "full":
			return m.access_level_full();
		case "controller":
			return m.access_level_controller();
		case "view":
			return m.access_level_view();
		case "custom":
			return m.access_level_custom();
	}
};

/**
 * The expiry the operator is choosing. "keep" only exists in the edit sheet (a device already
 * has an expiry and the PATCH is partial — omitting the field keeps it).
 */
export type ExpiryChoice =
	| "keep"
	| "forever"
	| "session"
	| "1h"
	| "4h"
	| "8h"
	| "custom";

export interface AccessDraft {
	/** Grant bitmask (`GRANT_*`). */
	grants: number;
	expiry: ExpiryChoice;
	/** Only read when `expiry === "custom"`. */
	customHours: number;
}

/**
 * `until_disconnect` for the drafted expiry: the record goes when the device's last session
 * does, instead of at a clock time. It is one choice among the expiry options here, so a draft
 * that says "session" carries no deadline — the API would take both and end the grant on
 * whichever landed first, but nothing in this console sends both.
 */
export const draftUntilDisconnect = (draft: AccessDraft): boolean =>
	draft.expiry === "session";

/** Relative seconds for the drafted expiry, or null for forever/keep/session (field omitted). */
export const draftExpirySecs = (draft: AccessDraft): number | null => {
	switch (draft.expiry) {
		case "1h":
			return 3600;
		case "4h":
			return 4 * 3600;
		case "8h":
			return 8 * 3600;
		case "custom":
			return Math.max(1, Math.round(draft.customHours)) * 3600;
		default:
			return null;
	}
};

/**
 * Pre-fill a draft from a stored record (the expired-guest re-knock: "re-grant what they had").
 * The stored expiry is ABSOLUTE and usually already past, so the previous DURATION is what gets
 * re-offered — recovered from `expires - granted` when both are known, else Forever.
 */
export const draftFromStored = (
	grants: number | null | undefined,
	expiresUnix: number | null | undefined,
	grantedUnix: number | null | undefined,
	untilDisconnect?: boolean | null,
): AccessDraft => {
	// null grants = a pre-grants record = full control (the API contract); an explicit
	// pre-power full mask normalizes so the toggles agree with the "Full" chip.
	const mask = normalizeLegacyFull(grants ?? GRANT_ALL);
	// "This session" outranks a deadline in the display: it is the one that will actually end
	// the grant, and the two are not exclusive on the wire.
	if (untilDisconnect)
		return { grants: mask, expiry: "session", customHours: 4 };
	if (expiresUnix == null || grantedUnix == null || expiresUnix <= grantedUnix)
		return { grants: mask, expiry: "forever", customHours: 4 };
	const secs = expiresUnix - grantedUnix;
	const expiry: ExpiryChoice =
		secs === 3600
			? "1h"
			: secs === 4 * 3600
				? "4h"
				: secs === 8 * 3600
					? "8h"
					: "custom";
	return {
		grants: mask,
		expiry,
		customHours: Math.max(1, Math.round(secs / 3600)),
	};
};

/**
 * The wall clock, ticking on ONE shared interval per caller — every countdown in a list derives
 * from this single value, so a page of rows re-renders once per tick and never refetches.
 */
export const useNowUnix = (stepMs = 30_000): number => {
	const [now, setNow] = useState(() => Math.floor(Date.now() / 1000));
	useEffect(() => {
		const t = setInterval(() => setNow(Math.floor(Date.now() / 1000)), stepMs);
		return () => clearInterval(t);
	}, [stepMs]);
	return now;
};

/** Remaining seconds → a short "left" label ("2 h left"); the caller has ruled out ≤ 0. */
export const fmtRemaining = (secs: number): string => {
	if (secs >= 48 * 3600)
		return m.access_left_days({ d: Math.round(secs / 86400) });
	if (secs >= 3600) return m.access_left_hours({ h: Math.round(secs / 3600) });
	if (secs >= 60) return m.access_left_minutes({ min: Math.ceil(secs / 60) });
	return m.access_left_under_minute();
};

/**
 * The Access chip: preset label + live countdown ("Controller · 2 h left"), or "Expired" once
 * the deadline passed (the row stays listed — D3). `grants` may be null (pre-grants = full).
 */
export const AccessChip: FC<{
	grants: number | null | undefined;
	expiresUnix: number | null | undefined;
	nowUnix: number;
	/** The record goes when this device's last session does — no deadline to count down. */
	untilDisconnect?: boolean | null;
}> = ({ grants, expiresUnix, nowUnix, untilDisconnect }) => {
	// "Expired" is the reader's arithmetic against the wall clock — the host keeps the row.
	if (expiresUnix != null && expiresUnix <= nowUnix)
		return <Badge variant="warning">{m.access_expired()}</Badge>;
	const label = levelLabel(levelOfMask((grants ?? GRANT_ALL) & GRANT_ALL));
	// A session grant has no deadline, so without this it would read as permanent — the one
	// reading it is deciding whether anything still needs cleaning up after the evening.
	const when = untilDisconnect
		? m.access_until_disconnect()
		: expiresUnix == null
			? null
			: fmtRemaining(expiresUnix - nowUnix);
	return (
		<Badge variant="secondary" className="whitespace-nowrap">
			{when == null ? label : `${label} · ${when}`}
		</Badge>
	);
};

/** The toggles behind Advanced, in bit order. Labels name what the bit covers. */
const GRANT_TOGGLES: {
	bit: number;
	label: () => string;
	/** One line under the label, for a bit whose reach isn't obvious from its name alone. */
	caption?: () => string;
}[] = [
	{ bit: GRANT_GAMEPAD, label: () => m.access_grant_gamepad() },
	{ bit: GRANT_POINTER, label: () => m.access_grant_pointer() },
	{ bit: GRANT_KEYBOARD, label: () => m.access_grant_keyboard() },
	{ bit: GRANT_CLIPBOARD, label: () => m.access_grant_clipboard() },
	{ bit: GRANT_MIC, label: () => m.access_grant_mic() },
	{ bit: GRANT_LAUNCH, label: () => m.access_grant_launch() },
	{
		bit: GRANT_POWER,
		label: () => m.access_grant_power(),
		caption: () => m.access_grant_power_caption(),
	},
	{
		bit: GRANT_MANAGE_GAMES,
		label: () => m.access_grant_manage_games(),
		caption: () => m.access_grant_manage_games_caption(),
	},
];

/**
 * The shared access controls: Access level (three presets + an Advanced expander with the
 * grant toggles) and Access expires (Forever / 1 h / 4 h / 8 h / custom). Three grant moments,
 * one component — approve dialog, arm card, edit sheet.
 *
 * The two sit side by side in an `@container` with room for both (the arm card alone on its row)
 * and stack everywhere else. The dialogs declare no container, so they always stack.
 */
export const AccessControls: FC<{
	value: AccessDraft;
	onChange: (next: AccessDraft) => void;
	/** Distinct control ids when two instances could mount at once. */
	idPrefix: string;
	/** Edit sheet: the device already has an expiry — offer "Keep current expiry" (= omit). */
	allowKeepExpiry?: boolean;
}> = ({ value, onChange, idPrefix, allowKeepExpiry }) => {
	// The expander opens itself when the mask is already custom (a prefilled re-grant) — a
	// "Custom" select over six hidden toggles would name a state it doesn't show.
	const [advanced, setAdvanced] = useState(
		() => levelOfMask(value.grants) === "custom",
	);
	const level = levelOfMask(value.grants);

	const setLevel = (next: string) => {
		if (next === "full") onChange({ ...value, grants: GRANT_ALL });
		else if (next === "controller")
			onChange({ ...value, grants: PRESET_CONTROLLER });
		else if (next === "view") onChange({ ...value, grants: PRESET_VIEW });
		// "custom" is display-only — it appears when the toggles made the mask custom.
	};

	return (
		<div className="grid items-start gap-4 @xl:grid-cols-2">
			<div className="space-y-2">
				<Label htmlFor={`${idPrefix}-level`}>{m.access_level_label()}</Label>
				<Select value={level} onValueChange={setLevel}>
					<SelectTrigger id={`${idPrefix}-level`}>
						<SelectValue />
					</SelectTrigger>
					<SelectContent>
						<SelectItem value="full">{m.access_level_full()}</SelectItem>
						<SelectItem value="controller">
							{m.access_level_controller()}
						</SelectItem>
						<SelectItem value="view">{m.access_level_view()}</SelectItem>
						{/* Only mounted while the toggles hold a non-preset mask — "Custom" is a
						    state the presets can't express, not a preset to pick. */}
						{level === "custom" && (
							<SelectItem value="custom">{m.access_level_custom()}</SelectItem>
						)}
					</SelectContent>
				</Select>

				<button
					type="button"
					className="flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground"
					aria-expanded={advanced}
					onClick={() => setAdvanced((v) => !v)}
				>
					{advanced ? (
						<ChevronDown className="size-3" />
					) : (
						<ChevronRight className="size-3" />
					)}
					{m.access_advanced()}
				</button>
				{advanced && (
					<div className="grid grid-cols-2 gap-x-4 gap-y-2 rounded-md border p-3">
						{GRANT_TOGGLES.map(({ bit, label, caption }) => (
							<Label
								key={bit}
								className="flex items-center gap-2 text-sm font-normal"
							>
								<Checkbox
									checked={(value.grants & bit) !== 0}
									onCheckedChange={(next) =>
										onChange({
											...value,
											grants:
												next === true
													? value.grants | bit
													: value.grants & ~bit,
										})
									}
								/>
								{caption ? (
									<span className="flex flex-col">
										{label()}
										<span className="text-xs text-muted-foreground">
											{caption()}
										</span>
									</span>
								) : (
									label()
								)}
							</Label>
						))}
					</div>
				)}
			</div>

			<div className="space-y-2">
				<Label htmlFor={`${idPrefix}-expires`}>
					{m.access_expires_label()}
				</Label>
				<Select
					value={value.expiry}
					onValueChange={(expiry) =>
						onChange({ ...value, expiry: expiry as ExpiryChoice })
					}
				>
					<SelectTrigger id={`${idPrefix}-expires`}>
						<SelectValue />
					</SelectTrigger>
					<SelectContent>
						{allowKeepExpiry && (
							<SelectItem value="keep">{m.access_expires_keep()}</SelectItem>
						)}
						<SelectItem value="forever">
							{m.access_expires_forever()}
						</SelectItem>
						<SelectItem value="session">
							{m.access_expires_session()}
						</SelectItem>
						<SelectItem value="1h">{m.access_expires_1h()}</SelectItem>
						<SelectItem value="4h">{m.access_expires_4h()}</SelectItem>
						<SelectItem value="8h">{m.access_expires_8h()}</SelectItem>
						<SelectItem value="custom">{m.access_expires_custom()}</SelectItem>
					</SelectContent>
				</Select>
				{value.expiry === "custom" && (
					<div className="space-y-2">
						<Label htmlFor={`${idPrefix}-hours`}>
							{m.access_expires_custom_hours()}
						</Label>
						<InputNumber
							id={`${idPrefix}-hours`}
							min={1}
							max={24 * 30}
							value={value.customHours}
							onChange={(customHours) => onChange({ ...value, customHours })}
						/>
					</div>
				)}
			</div>
		</div>
	);
};
