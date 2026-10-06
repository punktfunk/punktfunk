import Section from "@unom/ui/section";
import {
	Check,
	CircleX,
	ImageMinus,
	ImagePlus,
	Pencil,
	Plus,
	Trash2,
	TriangleAlert,
} from "lucide-react";
import { type FC, useEffect, useRef, useState } from "react";
import type { ProfileAdmin } from "@/api/gen/model/profileAdmin";
import type { ProfileCreate } from "@/api/gen/model/profileCreate";
import type { Seating } from "@/api/gen/model/seating";
import type { SeatState } from "@/api/gen/model/seatState";
import {
	PasswordConfirmField,
	type PasswordFailure,
} from "@/components/password-confirm";
import { ProfileAvatar } from "@/components/profile-avatar";
import { QueryState } from "@/components/query-state";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
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
import { fmtDateTimeSecs } from "@/lib/format";
import type { Loadable } from "@/lib/query";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";

/** The colours **Add profile** offers. The host takes any `#RRGGBB`. */
export const ACCENTS = [
	"#3b82f6",
	"#f97316",
	"#22c55e",
	"#a855f7",
	"#ec4899",
	"#eab308",
	"#14b8a6",
	"#ef4444",
] as const;

const STATE_WORD: Record<SeatState, () => string> = {
	ready: () => m.profiles_state_ready(),
	starting: () => m.profiles_state_starting(),
	stopped: () => m.profiles_state_stopped(),
	occupied: () => m.profiles_state_occupied(),
	unavailable: () => m.profiles_state_unavailable(),
};

/**
 * **Steam per seat** on this host: on, off until the first Own Steam profile turns it on, or
 * off because the operator set it so. While off, an Own Steam profile plays on the owner's Steam.
 */
export type SeatHome = "on" | "turns-on" | "off";

/** Where a profile plays, as one line: the owner's desktop, a share of it, or its own seat. */
export function playsLine(
	p: ProfileAdmin,
	ownerName: string,
	seatHome: SeatHome = "on",
	windows = false,
): string {
	if (p.owner) return m.profiles_plays_owner();
	const seat = p.seat;
	if (!seat) return m.profiles_plays_shared({ owner: ownerName });
	if (seat.state === "occupied" && seat.occupant)
		return m.profiles_occupied_by({ device: seat.occupant });
	if (seat.state === "starting")
		return seat.detail || m.profiles_seat_starting();
	if (seat.state === "unavailable")
		return seat.detail || STATE_WORD.unavailable();
	if (windows) return m.profiles_outcome_desktop();
	if (seatHome !== "on") return m.profiles_plays_steam_off();
	if (seat.steam_sign_in === true) return m.profiles_plays_steam_sign_in();
	if (seat.steam_sign_in === false) return m.profiles_plays_steam_signed_in();
	return m.profiles_plays_steam();
}

/** The doctor's line under the cards. `message` is its first error; `null` is healthy. */
export type DoctorLine = {
	message: string | null;
	checking: boolean;
	onRun: () => void;
};

/** What a Windows host adds to the page: **Seats**, a seat's buttons and the doctor's line. */
export type WindowsSeats = {
	onSeats: () => void;
	onStart: (p: ProfileAdmin) => void;
	onStop: (p: ProfileAdmin) => void;
	onEnd: (p: ProfileAdmin) => void;
	/** `null` until the doctor has answered. */
	doctor: DoctorLine | null;
};

/** The page: one card per profile, the owner first, and **Add profile**. */
export const ProfilesView: FC<{
	profiles: Loadable<ProfileAdmin[]>;
	/** Bumped per profile after a picture upload, so the new one shows. */
	avatarVersions?: Record<string, number>;
	onAdd: () => void;
	onRename: (p: ProfileAdmin) => void;
	onPicture: (p: ProfileAdmin, file: File) => void;
	onRemovePicture: (p: ProfileAdmin) => void;
	onRemove: (p: ProfileAdmin) => void;
	/** The profile an edit is in flight for; its buttons wait. */
	busyId: string | null;
	seatHome?: SeatHome;
	windows?: WindowsSeats;
}> = ({
	profiles,
	avatarVersions,
	onAdd,
	onRename,
	onPicture,
	onRemovePicture,
	onRemove,
	busyId,
	seatHome = "on",
	windows,
}) => {
	const list = profiles.data ?? [];
	const ownerName = list.find((p) => p.owner)?.display_name ?? "";
	return (
		<Section maxWidth={false}>
			<div className="flex flex-col gap-card">
				<div className="flex items-center justify-between gap-4">
					<h1 className="text-2xl font-semibold">{m.profiles_title()}</h1>
					<div className="flex gap-2">
						{windows && (
							<Button variant="outline" onClick={windows.onSeats}>
								{m.profiles_seats()}
							</Button>
						)}
						<Button onClick={onAdd} disabled={profiles.data == null}>
							<Plus className="size-4" />
							{m.profiles_add()}
						</Button>
					</div>
				</div>
				<QueryState
					isLoading={profiles.isLoading}
					error={profiles.error}
					refetch={profiles.refetch}
				>
					<Card>
						<CardContent className="flex flex-col gap-4 pt-6">
							{list.map((p) => (
								<ProfileRow
									key={p.id}
									profile={p}
									ownerName={ownerName}
									seatHome={seatHome}
									windows={windows}
									version={avatarVersions?.[p.id]}
									busy={busyId === p.id}
									onRename={() => onRename(p)}
									onPicture={(f) => onPicture(p, f)}
									onRemovePicture={() => onRemovePicture(p)}
									onRemove={() => onRemove(p)}
								/>
							))}
						</CardContent>
					</Card>
					{windows?.doctor && <DoctorFooter line={windows.doctor} />}
				</QueryState>
			</div>
		</Section>
	);
};

const ProfileRow: FC<{
	profile: ProfileAdmin;
	ownerName: string;
	seatHome: SeatHome;
	windows?: WindowsSeats;
	version?: number;
	busy: boolean;
	onRename: () => void;
	onPicture: (file: File) => void;
	onRemovePicture: () => void;
	onRemove: () => void;
}> = ({
	profile: p,
	ownerName,
	seatHome,
	windows,
	version,
	busy,
	onRename,
	onPicture,
	onRemovePicture,
	onRemove,
}) => {
	const file = useRef<HTMLInputElement>(null);
	const state = p.seat?.state;
	const facts = [
		playsLine(p, ownerName, seatHome, windows != null),
		p.home === "bigpicture"
			? m.profiles_home_bigpicture()
			: m.profiles_home_desktop(),
		p.last_used_unix > 0
			? m.profiles_last_used({ when: fmtDateTimeSecs(p.last_used_unix) })
			: m.profiles_never_used(),
	];
	return (
		<div className="flex flex-col gap-3 border-b pb-4 last:border-0 last:pb-0 sm:flex-row sm:items-center">
			<div className="flex min-w-0 flex-1 items-center gap-3">
				<ProfileAvatar profile={p} version={version} className="size-11" />
				<div className="min-w-0">
					<div className="flex flex-wrap items-center gap-2">
						<span className="truncate font-medium">{p.display_name}</span>
						{p.owner && <Badge variant="secondary">{m.profiles_owner()}</Badge>}
						{p.seat && (
							<Badge
								variant={
									p.seat.state === "unavailable" ? "destructive" : "outline"
								}
							>
								{STATE_WORD[p.seat.state]()}
							</Badge>
						)}
					</div>
					<p className="text-sm text-muted-foreground">{facts.join(" · ")}</p>
				</div>
			</div>
			<div className="flex shrink-0 items-center justify-end gap-1">
				{windows && state === "stopped" && (
					<Button
						variant="outline"
						size="sm"
						disabled={busy}
						onClick={() => windows.onStart(p)}
					>
						{m.profiles_seat_start()}
					</Button>
				)}
				{windows && state === "occupied" && (
					<Button
						variant="outline"
						size="sm"
						disabled={busy}
						onClick={() => windows.onEnd(p)}
					>
						{m.profiles_seat_end()}
					</Button>
				)}
				{windows &&
					(state === "ready" ||
						state === "occupied" ||
						state === "starting") && (
						<Button
							variant="outline"
							size="sm"
							disabled={busy}
							onClick={() => windows.onStop(p)}
						>
							{m.profiles_seat_stop()}
						</Button>
					)}
				<Button
					variant="ghost"
					size="icon"
					aria-label={m.action_rename()}
					title={m.action_rename()}
					disabled={busy}
					onClick={onRename}
				>
					<Pencil className="size-4" />
				</Button>
				<input
					ref={file}
					type="file"
					accept="image/png,image/jpeg"
					className="hidden"
					onChange={(e) => {
						const picked = e.target.files?.[0];
						e.target.value = "";
						if (picked) onPicture(picked);
					}}
				/>
				<Button
					variant="ghost"
					size="icon"
					aria-label={m.profiles_picture_set()}
					title={m.profiles_picture_set()}
					disabled={busy}
					onClick={() => file.current?.click()}
				>
					<ImagePlus className="size-4" />
				</Button>
				{p.avatar && (
					<Button
						variant="ghost"
						size="icon"
						aria-label={m.profiles_picture_remove()}
						title={m.profiles_picture_remove()}
						disabled={busy}
						onClick={onRemovePicture}
					>
						<ImageMinus className="size-4" />
					</Button>
				)}
				{!p.owner && (
					<Button
						variant="ghost"
						size="icon"
						aria-label={m.profiles_remove()}
						title={m.profiles_remove()}
						disabled={busy}
						onClick={onRemove}
					>
						<Trash2 className="size-4 text-destructive" />
					</Button>
				)}
			</div>
		</div>
	);
};

/** One answer to where a new profile plays. `seats` is a desktop of its own: Windows seats. */
type Outcome = "shared" | "steam" | "seats";

/**
 * **Add profile**: a name, a colour, a picture, then one question answered as outcomes. The home
 * follows the answer on the host.
 */
export const AddProfileDialog: FC<{
	open: boolean;
	ownerName: string;
	/** A light seat (own Steam) is a Linux host's. */
	linux: boolean;
	/** A desktop of its own is a Windows host's, once its seats are on. */
	windows?: boolean;
	seatsOn?: boolean;
	seatHome?: SeatHome;
	onCancel: () => void;
	onCreate: (body: ProfileCreate, picture: File | null) => void;
	isPending: boolean;
}> = ({
	open,
	ownerName,
	linux,
	windows = false,
	seatsOn = false,
	seatHome = "on",
	onCancel,
	onCreate,
	isPending,
}) => {
	const [name, setName] = useState("");
	const [accent, setAccent] = useState<string>(ACCENTS[1]);
	const [picture, setPicture] = useState<File | null>(null);
	const [outcome, setOutcome] = useState<Outcome>(linux ? "steam" : "shared");
	useEffect(() => {
		if (!open) return;
		setName("");
		setAccent(ACCENTS[1]);
		setPicture(null);
		setOutcome(linux ? "steam" : "shared");
	}, [open, linux]);
	const submit = () => {
		if (!name.trim()) return;
		onCreate(
			{
				display_name: name.trim(),
				accent,
				seat: outcome === "steam" || outcome === "seats",
			},
			picture,
		);
	};
	const outcomes: {
		id: Outcome;
		label: string;
		hint: string;
		disabled: boolean;
	}[] = [
		{
			id: "shared",
			label: m.profiles_outcome_shared({ owner: ownerName }),
			hint: m.profiles_outcome_shared_hint(),
			disabled: false,
		},
		{
			id: "steam",
			label: m.profiles_plays_steam(),
			hint: !linux
				? m.profiles_outcome_needs_linux()
				: {
						on: m.profiles_outcome_steam_hint,
						"turns-on": m.profiles_outcome_steam_hint_turns_on,
						off: m.profiles_outcome_steam_hint_off,
					}[seatHome](),
			disabled: !linux,
		},
		{
			id: "seats",
			label: m.profiles_outcome_desktop(),
			hint: !windows
				? m.profiles_outcome_needs_seats()
				: seatsOn
					? m.profiles_outcome_desktop_hint()
					: m.profiles_outcome_seats_off(),
			disabled: !windows || !seatsOn,
		},
	];
	return (
		<Dialog open={open} onOpenChange={(o) => !o && onCancel()}>
			<DialogContent className="max-w-md">
				<DialogHeader>
					<DialogTitle>{m.profiles_add()}</DialogTitle>
				</DialogHeader>
				<form
					className="flex flex-col gap-4"
					onSubmit={(e) => {
						e.preventDefault();
						if (!isPending) submit();
					}}
				>
					<div className="space-y-2">
						<Label htmlFor="profile-name">{m.profiles_name()}</Label>
						<Input
							id="profile-name"
							autoFocus
							autoComplete="off"
							maxLength={32}
							value={name}
							onChange={(e) => setName(e.target.value)}
						/>
					</div>
					<div className="flex items-center gap-4">
						<ProfileAvatar
							profile={{
								id: "new",
								display_name: name || "?",
								accent,
							}}
							className="size-11"
						/>
						<fieldset className="flex flex-wrap gap-2">
							<legend className="sr-only">{m.profiles_colour()}</legend>
							{ACCENTS.map((c) => (
								<button
									key={c}
									type="button"
									aria-label={c}
									aria-pressed={accent === c}
									onClick={() => setAccent(c)}
									className={cn(
										"size-6 rounded-full ring-offset-2 ring-offset-background",
										accent === c && "ring-2 ring-foreground",
									)}
									style={{ backgroundColor: c }}
								/>
							))}
						</fieldset>
					</div>
					<div className="space-y-2">
						<Label htmlFor="profile-picture">{m.profiles_picture_set()}</Label>
						<Input
							id="profile-picture"
							type="file"
							accept="image/png,image/jpeg"
							onChange={(e) => setPicture(e.target.files?.[0] ?? null)}
						/>
					</div>
					<fieldset className="flex flex-col gap-2">
						<legend className="mb-2 text-sm font-medium">
							{m.profiles_plays_question()}
						</legend>
						{outcomes.map((o) => (
							<label
								key={o.id}
								className={cn(
									"flex cursor-pointer gap-3 rounded-md border p-3",
									outcome === o.id && "border-primary bg-primary/5",
									o.disabled && "cursor-not-allowed opacity-50",
								)}
							>
								<input
									type="radio"
									name="profile-outcome"
									className="mt-1"
									value={o.id}
									checked={outcome === o.id}
									disabled={o.disabled}
									onChange={() => setOutcome(o.id)}
								/>
								<span>
									<span className="block text-sm font-medium">{o.label}</span>
									<span className="block text-xs text-muted-foreground">
										{o.hint}
									</span>
								</span>
							</label>
						))}
					</fieldset>
					<DialogFooter>
						<Button
							type="button"
							variant="outline"
							onClick={onCancel}
							disabled={isPending}
						>
							{m.common_cancel()}
						</Button>
						<Button type="submit" disabled={isPending || !name.trim()}>
							{m.profiles_add()}
						</Button>
					</DialogFooter>
				</form>
			</DialogContent>
		</Dialog>
	);
};

/**
 * **Remove**, behind the console password. `erase` is offered where a seat home can stay; a
 * Windows seat's account always goes with it.
 */
export const RemoveProfileDialog: FC<{
	profile: ProfileAdmin | null;
	onCancel: () => void;
	onRemove: (id: string, erase: boolean, password: string) => void;
	isPending: boolean;
	failure: PasswordFailure;
	windows?: boolean;
}> = ({ profile, onCancel, onRemove, isPending, failure, windows = false }) => {
	const [erase, setErase] = useState(false);
	const [password, setPassword] = useState("");
	useEffect(() => {
		if (!profile) return;
		setErase(false);
		setPassword("");
	}, [profile]);
	const windowsSeat = windows && profile?.seat != null;
	const submit = () => {
		if (profile && password)
			onRemove(profile.id, erase || windowsSeat, password);
	};
	return (
		<Dialog open={profile !== null} onOpenChange={(o) => !o && onCancel()}>
			{profile && (
				<DialogContent className="max-w-md">
					<DialogHeader>
						<DialogTitle>
							{m.profiles_remove_title({ name: profile.display_name })}
						</DialogTitle>
						<DialogDescription>
							{m.profiles_remove_body({ name: profile.display_name })}
							{windowsSeat &&
								` ${m.profiles_remove_windows({ name: profile.display_name })}`}
						</DialogDescription>
					</DialogHeader>
					{profile.seat && !windowsSeat && (
						<div className="flex items-start gap-2">
							<Checkbox
								id="profile-erase"
								checked={erase}
								onCheckedChange={(v) => setErase(v === true)}
							/>
							<Label htmlFor="profile-erase" className="leading-snug">
								{m.profiles_remove_erase({ name: profile.display_name })}
							</Label>
						</div>
					)}
					<form
						onSubmit={(e) => {
							e.preventDefault();
							if (!isPending) submit();
						}}
					>
						<PasswordConfirmField
							id="profile-remove-password"
							value={password}
							onChange={setPassword}
							failure={failure}
							autoFocus
						/>
					</form>
					<DialogFooter>
						<Button variant="outline" onClick={onCancel} disabled={isPending}>
							{m.common_cancel()}
						</Button>
						<Button
							variant="destructive"
							disabled={isPending || password.length === 0}
							onClick={submit}
						>
							{m.profiles_remove()}
						</Button>
					</DialogFooter>
				</DialogContent>
			)}
		</Dialog>
	);
};

/** The doctor's verdict under the cards: quiet when healthy, its first error otherwise. */
const DoctorFooter: FC<{ line: DoctorLine }> = ({ line }) => (
	<div className="flex items-center justify-between gap-4 text-sm text-muted-foreground">
		<span className={cn(line.message && "text-destructive")}>
			{line.message ?? m.profiles_seats_healthy()}
		</span>
		<Button
			variant="outline"
			size="sm"
			disabled={line.checking}
			onClick={line.onRun}
		>
			{m.profiles_seats_doctor()}
		</Button>
	</div>
);

const LEVEL = {
	error: { rank: 0, Icon: CircleX, tone: "text-destructive" },
	warning: { rank: 1, Icon: TriangleAlert, tone: "text-[var(--warning)]" },
	info: { rank: 2, Icon: Check, tone: "text-muted-foreground" },
} as const;

/**
 * **Seats**: the switch, the Remote Desktop choice a turn-on carries, and what the checks found.
 * A refused turn-on answers off with its errors, which is what the list shows.
 */
export const SeatsDialog: FC<{
	open: boolean;
	seating: Seating | undefined;
	isPending: boolean;
	onChange: (enabled: boolean, allowRdp: boolean) => void;
	onClose: () => void;
}> = ({ open, seating, isPending, onChange, onClose }) => {
	const [allowRdp, setAllowRdp] = useState(false);
	useEffect(() => {
		if (open) setAllowRdp(false);
	}, [open]);
	const on = seating?.enabled === true;
	const checks = [...(seating?.checks ?? [])].sort(
		(a, b) => LEVEL[a.level].rank - LEVEL[b.level].rank,
	);
	return (
		<Dialog open={open} onOpenChange={(o) => !o && onClose()}>
			<DialogContent className="max-w-md">
				<DialogHeader>
					<DialogTitle>{m.profiles_seats()}</DialogTitle>
				</DialogHeader>
				<div className="flex items-center gap-2">
					<Checkbox
						id="seats-on"
						checked={on}
						disabled={isPending || !seating}
						onCheckedChange={(v) => onChange(v === true, allowRdp)}
					/>
					<Label htmlFor="seats-on">{m.profiles_seats_on()}</Label>
				</div>
				{!on && (
					<div className="flex items-start gap-2">
						<Checkbox
							id="seats-rdp"
							className="mt-0.5"
							checked={allowRdp}
							onCheckedChange={(v) => setAllowRdp(v === true)}
						/>
						<div className="space-y-1">
							<Label htmlFor="seats-rdp" className="leading-snug">
								{m.profiles_seats_rdp()}
							</Label>
							<p className="text-xs text-muted-foreground">
								{m.profiles_seats_rdp_hint()}
							</p>
						</div>
					</div>
				)}
				{checks.length > 0 && (
					<ul className="flex flex-col gap-1.5 text-sm">
						{checks.map((c, i) => {
							const { Icon, tone } = LEVEL[c.level];
							return (
								<li key={`${c.code}-${i}`} className="flex items-start gap-2">
									<Icon className={cn("mt-0.5 size-4 shrink-0", tone)} />
									<span>{c.message}</span>
								</li>
							);
						})}
					</ul>
				)}
				<DialogFooter>
					<Button onClick={onClose}>{m.common_done()}</Button>
				</DialogFooter>
			</DialogContent>
		</Dialog>
	);
};
