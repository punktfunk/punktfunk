import Section from "@unom/ui/section";
import {
	AppWindow,
	Check,
	CircleX,
	Gamepad2,
	ImageMinus,
	ImagePlus,
	Pencil,
	Plus,
	Trash2,
	TriangleAlert,
	Users,
} from "lucide-react";
import { motion } from "motion/react";
import { type FC, useEffect, useRef, useState } from "react";
import type { ProfileAdmin } from "@/api/gen/model/profileAdmin";
import type { ProfileCreate } from "@/api/gen/model/profileCreate";
import type { Seating } from "@/api/gen/model/seating";
import type { SeatState } from "@/api/gen/model/seatState";
import { DocsLink } from "@/components/docs-link";
import {
	PasswordConfirmField,
	type PasswordFailure,
} from "@/components/password-confirm";
import { ProfileAvatar } from "@/components/profile-avatar";
import { QueryState } from "@/components/query-state";
import { ROW, Stagger } from "@/components/stagger";
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
import { Spinner } from "@/components/ui/spinner";
import { fmtDateTimeSecs } from "@/lib/format";
import type { Loadable } from "@/lib/query";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";
import {
	AccentPicker,
	type Choice,
	ChoiceCards,
	PicturePicker,
} from "./pickers";

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
	// A door gives the profiles on the box's own session a seat too, the owner's.
	if (!seat || seat.kind === "shared")
		return m.profiles_plays_shared({ owner: ownerName });
	if (seat.state === "occupied" && seat.occupant)
		return m.profiles_occupied_by({ device: seat.occupant });
	if (seat.state === "starting")
		return seat.detail || m.profiles_seat_starting();
	if (seat.state === "unavailable")
		return seat.detail || STATE_WORD.unavailable();
	if (windows || seat.kind === "desktop") return m.profiles_outcome_desktop();
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

/** What a host with seats adds to the page: a seat's buttons and the doctor's line, and on
 * Windows **Seats**. */
export type SeatActions = {
	/** Windows: opens **Seats**. A Linux door has its own switch instead. */
	onSeats?: () => void;
	onStart: (p: ProfileAdmin) => void;
	onStop: (p: ProfileAdmin) => void;
	onEnd: (p: ProfileAdmin) => void;
	/** `null` until the doctor has answered. */
	doctor: DoctorLine | null;
};

/** **Reachable without logging in**, the Linux switch that makes the box a door. */
export type DoorControl = {
	on: boolean;
	/** The switch is under way: the host that answers is changing. */
	changing: boolean;
	/** Opens the confirmation. */
	onChange: () => void;
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
	seats?: SeatActions;
	/** Linux: the door's switch. */
	door?: DoorControl;
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
	seats,
	door,
}) => {
	const list = profiles.data ?? [];
	const ownerName = list.find((p) => p.owner)?.display_name ?? "";
	return (
		<Section maxWidth={false}>
			<div className="flex flex-col gap-card">
				<div className="flex items-center justify-between gap-4">
					<h1 className="text-2xl font-semibold">{m.profiles_title()}</h1>
					<div className="flex gap-2">
						{seats?.onSeats && (
							<Button variant="outline" onClick={seats.onSeats}>
								{m.profiles_seats()}
							</Button>
						)}
						<Button onClick={onAdd} disabled={profiles.data == null}>
							<Plus className="size-4" />
							{m.profiles_add()}
						</Button>
					</div>
				</div>
				{door && <DoorRow door={door} />}
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
									seats={seats}
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
					{seats?.doctor && <DoctorFooter line={seats.doctor} />}
				</QueryState>
			</div>
		</Section>
	);
};

/** The door's switch and the one line of what it does. The change itself asks for the password. */
const DoorRow: FC<{ door: DoorControl }> = ({ door }) => (
	<div className="flex items-start justify-between gap-4 rounded-md border p-4">
		<div className="space-y-1">
			<Label htmlFor="door-switch" className="font-medium">
				{m.profiles_door()}
			</Label>
			<p className="text-sm text-muted-foreground">
				{m.profiles_door_hint()} <DocsLink path="profiles#on-linux" />
			</p>
			{door.on && (
				<p className="text-sm text-muted-foreground">
					{m.profiles_door_games()}
				</p>
			)}
		</div>
		{door.changing ? (
			<span className="flex items-center gap-2 text-sm text-muted-foreground">
				<Spinner className="size-4" />
				{m.profiles_door_switching()}
			</span>
		) : (
			<Checkbox
				id="door-switch"
				className="mt-1"
				checked={door.on}
				onCheckedChange={() => door.onChange()}
			/>
		)}
	</div>
);

/** The door's confirmation: what turning it on or off does, and the console password. */
export const DoorDialog: FC<{
	open: boolean;
	/** The state the switch is asked to take. */
	turningOn: boolean;
	isPending: boolean;
	failure: PasswordFailure;
	onConfirm: (password: string) => void;
	onCancel: () => void;
}> = ({ open, turningOn, isPending, failure, onConfirm, onCancel }) => {
	const [password, setPassword] = useState("");
	useEffect(() => {
		if (open) setPassword("");
	}, [open]);
	return (
		<Dialog open={open} onOpenChange={(o) => !o && onCancel()}>
			<DialogContent className="max-w-md">
				<DialogHeader>
					<DialogTitle>{m.profiles_door()}</DialogTitle>
					<DialogDescription>
						{turningOn ? m.profiles_door_hint() : m.profiles_door_off_hint()}{" "}
						<DocsLink path="profiles#on-linux" />
					</DialogDescription>
				</DialogHeader>
				<form
					onSubmit={(e) => {
						e.preventDefault();
						if (!isPending && password) onConfirm(password);
					}}
				>
					<PasswordConfirmField
						id="door-password"
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
						disabled={isPending || password.length === 0}
						onClick={() => onConfirm(password)}
					>
						{turningOn ? m.profiles_door_on() : m.profiles_door_off()}
					</Button>
				</DialogFooter>
			</DialogContent>
		</Dialog>
	);
};

const ProfileRow: FC<{
	profile: ProfileAdmin;
	ownerName: string;
	seatHome: SeatHome;
	seats?: SeatActions;
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
	seats,
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
		playsLine(p, ownerName, seatHome),
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
				{seats && state === "stopped" && (
					<Button
						variant="outline"
						size="sm"
						disabled={busy}
						onClick={() => seats.onStart(p)}
					>
						{m.profiles_seat_start()}
					</Button>
				)}
				{seats && state === "occupied" && (
					<Button
						variant="outline"
						size="sm"
						disabled={busy}
						onClick={() => seats.onEnd(p)}
					>
						{m.profiles_seat_end()}
					</Button>
				)}
				{seats &&
					(state === "ready" ||
						state === "occupied" ||
						state === "starting") && (
						<Button
							variant="outline"
							size="sm"
							disabled={busy}
							onClick={() => seats.onStop(p)}
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
 * **Add profile**: the picture beside a name and a colour, then one question answered as
 * outcomes. The home follows the answer on the host.
 */
export const AddProfileDialog: FC<{
	open: boolean;
	ownerName: string;
	/** A light seat (own Steam) is a Linux host's. */
	linux: boolean;
	/** A desktop of its own is a Windows host's, once its seats are on. */
	windows?: boolean;
	seatsOn?: boolean;
	/** ...or a Linux host's, once the door is on. */
	door?: boolean;
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
	door = false,
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
				// A Windows seat is always a desktop; on Linux it is the door's.
				desktop: outcome === "seats",
			},
			picture,
		);
	};
	const outcomes: Choice<Outcome>[] = [
		{
			id: "shared",
			icon: <Users />,
			label: m.profiles_outcome_shared({ owner: ownerName }),
			hint: m.profiles_outcome_shared_hint(),
			disabled: false,
		},
		{
			id: "steam",
			icon: <Gamepad2 />,
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
			icon: <AppWindow />,
			label: m.profiles_outcome_desktop(),
			hint: windows
				? seatsOn
					? m.profiles_outcome_desktop_hint()
					: m.profiles_outcome_seats_off()
				: linux
					? door
						? m.profiles_outcome_desktop_hint_linux()
						: m.profiles_outcome_door_off()
					: m.profiles_outcome_needs_seats(),
			disabled: windows ? !seatsOn : !(linux && door),
		},
	];
	return (
		<Dialog open={open} onOpenChange={(o) => !o && onCancel()}>
			<DialogContent className="max-w-md sm:max-w-lg">
				<DialogHeader>
					<DialogTitle>{m.profiles_add()}</DialogTitle>
				</DialogHeader>
				<form
					onSubmit={(e) => {
						e.preventDefault();
						if (!isPending) submit();
					}}
				>
					<Stagger className="flex flex-col gap-5">
						<motion.div variants={ROW} className="flex items-start gap-4">
							<PicturePicker
								name={name}
								accent={accent}
								picture={picture}
								onPick={setPicture}
							/>
							<div className="min-w-0 flex-1 space-y-4">
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
								<AccentPicker
									colours={ACCENTS}
									value={accent}
									onChange={setAccent}
								/>
							</div>
						</motion.div>
						<motion.div variants={ROW}>
							<ChoiceCards
								label={m.profiles_plays_question()}
								value={outcome}
								choices={outcomes}
								onChange={setOutcome}
							/>
						</motion.div>
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
					</Stagger>
				</form>
			</DialogContent>
		</Dialog>
	);
};

/**
 * **Remove**, behind the console password. `erase` is offered where a seat home can stay; the
 * account of a desktop of its own always goes with it.
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
	// A Windows seat is always a desktop of its own; on Linux the kind says so.
	const ownAccount =
		profile?.seat != null && (windows || profile.seat.kind === "desktop");
	const submit = () => {
		if (profile && password)
			onRemove(profile.id, erase || ownAccount, password);
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
							{ownAccount &&
								` ${(windows ? m.profiles_remove_windows : m.profiles_remove_linux)({ name: profile.display_name })}`}
						</DialogDescription>
					</DialogHeader>
					{profile.seat && !ownAccount && (
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
 * **Seats**: the switch, the Remote Desktop choice, and what the checks found. While seats are on,
 * changing the choice turns them on again with it. A refused turn-on answers off with its errors,
 * which is what the list shows.
 */
export const SeatsDialog: FC<{
	open: boolean;
	seating: Seating | undefined;
	isPending: boolean;
	onChange: (enabled: boolean, allowRdp: boolean) => void;
	onClose: () => void;
}> = ({ open, seating, isPending, onChange, onClose }) => {
	const [allowRdp, setAllowRdp] = useState(false);
	const stored = seating?.allow_rdp_from_network === true;
	useEffect(() => {
		if (open) setAllowRdp(stored);
	}, [open, stored]);
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
				{seating && (
					<div className="flex items-start gap-2">
						<Checkbox
							id="seats-rdp"
							className="mt-0.5"
							checked={allowRdp}
							disabled={isPending}
							onCheckedChange={(v) => {
								setAllowRdp(v === true);
								if (on) onChange(true, v === true);
							}}
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
