import Section from "@unom/ui/section";
import {
	AppWindow,
	Check,
	CircleX,
	ImageMinus,
	ImagePlus,
	KeyRound,
	Pencil,
	Plus,
	RotateCw,
	Trash2,
	TriangleAlert,
	UserPlus,
	UserRound,
	Users,
} from "lucide-react";
import { motion } from "motion/react";
import { type FC, useEffect, useRef, useState } from "react";
import type { ProfileAdmin } from "@/api/gen/model/profileAdmin";
import type { ProfileCreate } from "@/api/gen/model/profileCreate";
import type { SeatCheck } from "@/api/gen/model/seatCheck";
import type { Seating } from "@/api/gen/model/seating";
import type { SeatState } from "@/api/gen/model/seatState";
import type { SeatsDoctor } from "@/api/gen/model/seatsDoctor";
import { DocsLink } from "@/components/docs-link";
import { PasswordConfirmDialog } from "@/components/password-confirm";
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
	DialogFooter,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { Spinner } from "@/components/ui/spinner";
import { apiErrorMessage } from "@/lib/errors";
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

/** Where a profile plays, as one line: the owner's desktop, a share of it, or its own desktop. */
export function playsLine(p: ProfileAdmin, ownerName: string): string {
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
	return seat.take_turns
		? m.profiles_outcome_account()
		: m.profiles_outcome_desktop();
}

/** The doctor's line under the cards. `message` is its first error, `notes` its warnings. */
export type DoctorLine = {
	message: string | null;
	notes: number;
	/** Opens **Doctor** with every check. */
	onOpen: () => void;
};

/** What a host with seats adds to the page: a seat's buttons and the doctor's line, and on
 * Windows **Seats**. */
export type SeatActions = {
	/** Windows: opens **Seats**. A Linux door has its own switch instead. */
	onSeats?: () => void;
	onStart: (p: ProfileAdmin) => void;
	onStop: (p: ProfileAdmin) => void;
	onEnd: (p: ProfileAdmin) => void;
	/** Opens **Update password** for a seat on an existing Windows account. */
	onPassword?: (p: ProfileAdmin) => void;
	/** `null` until the doctor has answered. */
	doctor: DoctorLine | null;
};

/** **Reachable without logging in**, the Linux switch that makes the box a door. */
export type DoorControl = {
	on: boolean;
	/** The switch is under way: the host that answers is changing. */
	changing: boolean;
	/** What stops turning it on here, the host's sentence. The switch waits until it's fixed. */
	unavailable?: string | null;
	/** Opens the confirmation. */
	onChange: () => void;
};

/** The page: one card per profile, the owner first, and **Add profile**. */
export const ProfilesView: FC<{
	profiles: Loadable<ProfileAdmin[]>;
	onAdd: () => void;
	onRename: (p: ProfileAdmin) => void;
	onPicture: (p: ProfileAdmin, file: File) => void;
	onRemovePicture: (p: ProfileAdmin) => void;
	onRemove: (p: ProfileAdmin) => void;
	/** The profile an edit is in flight for; its buttons wait. */
	busyId: string | null;
	seats?: SeatActions;
	/** Linux: the door's switch. */
	door?: DoorControl;
}> = ({
	profiles,
	onAdd,
	onRename,
	onPicture,
	onRemovePicture,
	onRemove,
	busyId,
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
									seats={seats}
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
			{!door.on && door.unavailable && (
				<p className="text-sm text-amber-600 dark:text-amber-500">
					{door.unavailable}
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
				disabled={!door.on && !!door.unavailable}
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
	onConfirm: (password: string) => Promise<void>;
	onCancel: () => void;
}> = ({ open, turningOn, onConfirm, onCancel }) => (
	<PasswordConfirmDialog
		open={open}
		id="door-password"
		className="max-w-md"
		title={m.profiles_door()}
		body={
			<>
				{turningOn ? m.profiles_door_hint() : m.profiles_door_off_hint()}{" "}
				<DocsLink path="profiles#on-linux" />
			</>
		}
		submitLabel={turningOn ? m.profiles_door_on() : m.profiles_door_off()}
		failedText={m.profiles_door_failed()}
		onSubmit={onConfirm}
		onClose={onCancel}
	/>
);

const ProfileRow: FC<{
	profile: ProfileAdmin;
	ownerName: string;
	seats?: SeatActions;
	busy: boolean;
	onRename: () => void;
	onPicture: (file: File) => void;
	onRemovePicture: () => void;
	onRemove: () => void;
}> = ({
	profile: p,
	ownerName,
	seats,
	busy,
	onRename,
	onPicture,
	onRemovePicture,
	onRemove,
}) => {
	const file = useRef<HTMLInputElement>(null);
	const state = p.seat?.state;
	// A take-turns profile has no managed seat to start/stop by hand — the client drives the
	// switch. Its card shows no seat buttons.
	const managed = p.seat?.take_turns ? undefined : seats;
	const facts = [
		playsLine(p, ownerName),
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
				<ProfileAvatar profile={p} className="size-11" />
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
					{p.seat?.account && (
						<p className="text-sm text-muted-foreground">
							{m.profiles_seat_account_line({ account: p.seat.account })}
						</p>
					)}
				</div>
			</div>
			<div className="flex shrink-0 items-center justify-end gap-1">
				{managed && state === "stopped" && (
					<Button
						variant="outline"
						size="sm"
						disabled={busy}
						onClick={() => managed.onStart(p)}
					>
						{m.profiles_seat_start()}
					</Button>
				)}
				{managed && state === "occupied" && (
					<Button
						variant="outline"
						size="sm"
						disabled={busy}
						onClick={() => managed.onEnd(p)}
					>
						{m.profiles_seat_end()}
					</Button>
				)}
				{managed &&
					(state === "ready" ||
						state === "occupied" ||
						state === "starting") && (
						<Button
							variant="outline"
							size="sm"
							disabled={busy}
							onClick={() => managed.onStop(p)}
						>
							{m.profiles_seat_stop()}
						</Button>
					)}
				{managed?.onPassword && p.seat?.account && (
					<Button
						variant="ghost"
						size="icon"
						aria-label={m.profiles_seat_password_update()}
						title={m.profiles_seat_password_update()}
						disabled={busy}
						onClick={() => managed.onPassword?.(p)}
					>
						<KeyRound className="size-4" />
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

/** One answer to where a new profile plays. `seats` is a desktop of its own; `account` is a
 * Windows account taking the console in turn. */
type Outcome = "shared" | "seats" | "account";

/** The Windows account a desktop of its own signs in to: one punktfunk makes, or one of this PC's. */
type SeatAccount = "new" | "existing";

/**
 * **Add profile**: the picture beside a name and a colour, then one question answered as
 * outcomes. The home follows the answer on the host. A Windows desktop of its own asks a
 * second: a new account, or an existing one with its password.
 */
export const AddProfileDialog: FC<{
	open: boolean;
	ownerName: string;
	linux: boolean;
	/** A desktop of its own is a Windows host's, once its seats are on. */
	windows?: boolean;
	seatsOn?: boolean;
	/** ...or a Linux host's, once the door is on. */
	door?: boolean;
	/** This Windows runs one desktop at a time: seats need Windows Server. */
	needsServer?: boolean;
	/** Local Windows accounts no profile holds: a take-turns profile or a seat can take one. */
	windowsAccounts?: readonly string[];
	/** Colours other profiles have: the form starts on the first free one. */
	takenAccents?: readonly string[];
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
	needsServer = false,
	windowsAccounts = [],
	takenAccents = [],
	onCancel,
	onCreate,
	isPending,
}) => {
	const freeAccent = firstFreeAccent(takenAccents);
	const [name, setName] = useState("");
	const [accent, setAccent] = useState<string>(freeAccent);
	const [picture, setPicture] = useState<File | null>(null);
	const [outcome, setOutcome] = useState<Outcome>("shared");
	const [account, setAccount] = useState<string>("");
	const [seatAccount, setSeatAccount] = useState<SeatAccount>("new");
	const [password, setPassword] = useState("");
	useEffect(() => {
		if (!open) return;
		setName("");
		setAccent(freeAccent);
		setPicture(null);
		setOutcome("shared");
		setAccount("");
		setSeatAccount("new");
		setPassword("");
	}, [open, freeAccent]);
	const asksAccount = windows && seatsOn && outcome === "seats";
	const adopting = asksAccount && seatAccount === "existing";
	const accountReady =
		outcome === "account"
			? account !== ""
			: !adopting || (account !== "" && password !== "");
	const submit = () => {
		if (!name.trim() || !accountReady) return;
		onCreate(
			{
				display_name: name.trim(),
				accent,
				seat: outcome === "seats",
				...(outcome === "account" ? { windows_account: account } : {}),
				...(adopting ? { seat_account: account, seat_password: password } : {}),
			},
			picture,
		);
	};
	const seatAccounts: Choice<SeatAccount>[] = [
		{
			id: "new",
			icon: <UserPlus />,
			label: m.profiles_seat_account_new(),
			hint: m.profiles_seat_account_new_hint(),
		},
		{
			id: "existing",
			icon: <UserRound />,
			label: m.profiles_seat_account_existing(),
			hint:
				windowsAccounts.length > 0
					? m.profiles_seat_account_existing_hint()
					: m.profiles_seat_account_none(),
			disabled: windowsAccounts.length === 0,
		},
	];
	const outcomes: Choice<Outcome>[] = [
		{
			id: "shared",
			icon: <Users />,
			label: m.profiles_outcome_shared({ owner: ownerName }),
			hint: m.profiles_outcome_shared_hint(),
			disabled: false,
		},
	];
	// Take turns is a Windows account on the one console — any edition, no seats.
	if (windows) {
		outcomes.push({
			id: "account",
			icon: <UserRound />,
			label: m.profiles_outcome_account(),
			hint:
				windowsAccounts.length > 0
					? m.profiles_outcome_account_hint()
					: m.profiles_outcome_account_none(),
			disabled: windowsAccounts.length === 0,
		});
	}
	outcomes.push({
		id: "seats",
		icon: <AppWindow />,
		label: m.profiles_outcome_desktop(),
		hint: windows
			? seatsOn
				? m.profiles_outcome_desktop_hint()
				: needsServer
					? m.profiles_outcome_needs_server()
					: m.profiles_outcome_seats_off()
			: linux
				? door
					? m.profiles_outcome_desktop_hint_linux()
					: m.profiles_outcome_door_off()
				: m.profiles_outcome_needs_seats(),
		disabled: windows ? !seatsOn : !(linux && door),
	});
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
						<motion.div variants={ROW} className="flex items-center gap-4">
							<PicturePicker
								name={name}
								accent={accent}
								picture={picture}
								onPick={setPicture}
							/>
							<div className="min-w-0 flex-1 space-y-2">
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
						</motion.div>
						{/* The whole width, so every swatch sits on one line. */}
						<motion.div variants={ROW}>
							<AccentPicker
								colours={ACCENTS}
								value={accent}
								onChange={setAccent}
							/>
						</motion.div>
						<motion.div variants={ROW}>
							<ChoiceCards
								label={m.profiles_plays_question()}
								value={outcome}
								choices={outcomes}
								onChange={setOutcome}
							/>
						</motion.div>
						{asksAccount && (
							<motion.div variants={ROW}>
								<ChoiceCards
									label={m.profiles_seat_account()}
									value={seatAccount}
									choices={seatAccounts}
									onChange={setSeatAccount}
								/>
							</motion.div>
						)}
						{(outcome === "account" || adopting) && (
							<motion.div variants={ROW} className="space-y-2">
								<Label htmlFor="profile-account">
									{m.profiles_account_which()}
								</Label>
								<Select value={account} onValueChange={setAccount}>
									<SelectTrigger
										id="profile-account"
										aria-label={m.profiles_account_which()}
									>
										<SelectValue placeholder={m.profiles_account_pick()} />
									</SelectTrigger>
									<SelectContent>
										{windowsAccounts.map((a) => (
											<SelectItem key={a} value={a}>
												{a}
											</SelectItem>
										))}
									</SelectContent>
								</Select>
								{!adopting && (
									<p className="text-sm text-muted-foreground">
										{m.profiles_account_turns()}
									</p>
								)}
							</motion.div>
						)}
						{adopting && (
							<motion.div variants={ROW} className="space-y-2">
								<Label htmlFor="profile-seat-password">
									{m.profiles_seat_password()}
								</Label>
								<Input
									id="profile-seat-password"
									type="password"
									autoComplete="off"
									data-1p-ignore
									value={password}
									onChange={(e) => setPassword(e.target.value)}
								/>
								<p className="text-sm text-muted-foreground">
									{m.profiles_seat_account_lock()}
								</p>
							</motion.div>
						)}
						<DialogFooter>
							<Button
								type="button"
								variant="outline"
								onClick={onCancel}
								disabled={isPending}
							>
								{m.common_cancel()}
							</Button>
							<Button
								type="submit"
								disabled={isPending || !name.trim() || !accountReady}
							>
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
 * **Remove**, behind the console password. `erase` is offered where this host keeps the
 * profile's Steam. A desktop of its own always sends it: the host removes the account it made,
 * and gives an existing one back with its files.
 */
export const RemoveProfileDialog: FC<{
	profile: ProfileAdmin | null;
	onCancel: () => void;
	onRemove: (id: string, erase: boolean, password: string) => Promise<void>;
	windows?: boolean;
}> = ({ profile, onCancel, onRemove, windows = false }) => {
	const [erase, setErase] = useState(false);
	useEffect(() => {
		if (profile) setErase(false);
	}, [profile]);
	if (!profile) return null;
	// A Windows seat is always a desktop of its own; on Linux the kind says so. A take-turns
	// profile binds to the player's own Windows account, which removal leaves alone.
	const ownAccount =
		profile.seat != null &&
		!profile.seat.take_turns &&
		(windows || profile.seat.kind === "desktop");
	const adopted = profile.seat?.account;
	const goes = adopted
		? m.profiles_remove_adopted({ account: adopted })
		: (windows ? m.profiles_remove_windows : m.profiles_remove_linux)({
				name: profile.display_name,
			});
	return (
		<PasswordConfirmDialog
			open
			id="profile-remove-password"
			className="max-w-md"
			title={m.profiles_remove_title({ name: profile.display_name })}
			body={
				<>
					{m.profiles_remove_body({ name: profile.display_name })}
					{ownAccount && ` ${goes}`}
				</>
			}
			submitLabel={m.profiles_remove()}
			failedText={m.profiles_remove_failed()}
			destructive
			onSubmit={(password) =>
				onRemove(profile.id, erase || ownAccount, password)
			}
			onClose={onCancel}
		>
			{profile.steam_home && !ownAccount && (
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
		</PasswordConfirmDialog>
	);
};

/**
 * **Update password**: the password a seat's existing Windows account signs in with now. A
 * refusal shows the host's sentence and keeps the dialog open.
 */
export const SeatPasswordDialog: FC<{
	profile: ProfileAdmin | null;
	onUpdate: (id: string, password: string) => Promise<unknown>;
	onClose: () => void;
}> = ({ profile, onUpdate, onClose }) => {
	const [password, setPassword] = useState("");
	const [busy, setBusy] = useState(false);
	const [error, setError] = useState<string | null>(null);
	// A close clears the typed password as well as an opening.
	// biome-ignore lint/correctness/useExhaustiveDependencies: `profile` is the open/close signal.
	useEffect(() => {
		setPassword("");
		setError(null);
	}, [profile]);
	const account = profile?.seat?.account;
	if (!profile || !account) return null;
	const submit = async () => {
		if (!password || busy) return;
		setBusy(true);
		setError(null);
		try {
			await onUpdate(profile.id, password);
			onClose();
		} catch (e) {
			setError(apiErrorMessage(e) ?? m.profiles_seat_password_failed());
		} finally {
			setBusy(false);
		}
	};
	return (
		<Dialog open onOpenChange={(o) => !o && !busy && onClose()}>
			<DialogContent className="max-w-md">
				<DialogHeader>
					<DialogTitle>
						{m.profiles_seat_password_title({ name: profile.display_name })}
					</DialogTitle>
				</DialogHeader>
				<form
					className="space-y-3"
					onSubmit={(e) => {
						e.preventDefault();
						void submit();
					}}
				>
					<Label htmlFor="seat-password">
						{m.profiles_seat_password_body({ account })}
					</Label>
					<Input
						id="seat-password"
						type="password"
						autoComplete="off"
						data-1p-ignore
						autoFocus
						value={password}
						onChange={(e) => setPassword(e.target.value)}
					/>
					{error && <p className="text-sm text-destructive">{error}</p>}
					<DialogFooter>
						<Button
							type="button"
							variant="outline"
							onClick={onClose}
							disabled={busy}
						>
							{m.common_cancel()}
						</Button>
						<Button type="submit" disabled={!password || busy}>
							{m.profiles_seat_password_save()}
						</Button>
					</DialogFooter>
				</form>
			</DialogContent>
		</Dialog>
	);
};

/** The doctor's verdict under the cards: its first error, else how many notes, else healthy. */
const DoctorFooter: FC<{ line: DoctorLine }> = ({ line }) => (
	<div className="flex items-center justify-between gap-4 text-sm text-muted-foreground">
		<span
			className={cn(
				line.message && "text-destructive",
				!line.message && line.notes > 0 && "text-[var(--warning)]",
			)}
		>
			{line.message ??
				(line.notes > 0
					? line.notes === 1
						? m.profiles_doctor_note()
						: m.profiles_doctor_notes({ count: line.notes })
					: m.profiles_seats_healthy())}
		</span>
		<Button variant="outline" size="sm" onClick={line.onOpen}>
			{m.profiles_seats_doctor()}
		</Button>
	</div>
);

/** The first palette colour no profile has, so two new profiles don't look alike. */
export const firstFreeAccent = (taken: readonly string[]): string => {
	const used = new Set(taken.map((c) => c.toLowerCase()));
	return ACCENTS.find((c) => !used.has(c.toLowerCase())) ?? ACCENTS[1];
};

const LEVEL = {
	error: { rank: 0, Icon: CircleX, tone: "text-destructive" },
	warning: { rank: 1, Icon: TriangleAlert, tone: "text-[var(--warning)]" },
	info: { rank: 2, Icon: Check, tone: "text-muted-foreground" },
} as const;

const byLevel = (a: SeatCheck, b: SeatCheck) =>
	LEVEL[a.level].rank - LEVEL[b.level].rank;

/** A restart Windows asks for blocks like an error and reads as the next step. */
const RESTART = { Icon: RotateCw, tone: "text-[var(--warning)]" } as const;

/** Checks as a list, the worst first. */
const CheckList: FC<{ checks: SeatCheck[] }> = ({ checks }) => (
	<ul className="flex flex-col gap-1.5 text-sm">
		{[...checks].sort(byLevel).map((c, i) => {
			const { Icon, tone } = c.code === "restart" ? RESTART : LEVEL[c.level];
			return (
				<li key={`${c.code}-${i}`} className="flex items-start gap-2">
					<Icon className={cn("mt-0.5 size-4 shrink-0", tone)} />
					<span>{c.message}</span>
				</li>
			);
		})}
	</ul>
);

/**
 * **Doctor**: every check the supervisor ran, the PC's first and then one group per seat, named
 * after the profile that plays there. Each line is the host's own sentence with the next move.
 */
export const DoctorDialog: FC<{
	open: boolean;
	doctor: SeatsDoctor | undefined;
	error: string | null;
	checking: boolean;
	/** The docs route its troubleshooting lives at. */
	docs: string;
	onRecheck: () => void;
	onClose: () => void;
}> = ({ open, doctor, error, checking, docs, onRecheck, onClose }) => {
	const rows = doctor?.diagnostics ?? [];
	const box = rows.filter((d) => !d.seat_id);
	const seats = [
		...new Set(rows.flatMap((d) => (d.seat_id ? [d.seat_id] : []))),
	];
	return (
		<Dialog open={open} onOpenChange={(o) => !o && onClose()}>
			<DialogContent className="max-h-[85vh] max-w-lg overflow-y-auto">
				<DialogHeader>
					<DialogTitle>{m.profiles_seats_doctor()}</DialogTitle>
				</DialogHeader>
				{error && <p className="text-sm text-destructive">{error}</p>}
				{box.length > 0 && (
					<section className="space-y-2">
						<h3 className="text-sm font-medium">{m.profiles_doctor_box()}</h3>
						<CheckList checks={box} />
					</section>
				)}
				{seats.map((id) => {
					const checks = rows.filter((d) => d.seat_id === id);
					return (
						<section key={id} className="space-y-2">
							<h3 className="text-sm font-medium">
								{checks.find((d) => d.seat_name)?.seat_name ??
									m.profiles_doctor_seat()}
							</h3>
							<CheckList checks={checks} />
						</section>
					);
				})}
				<DialogFooter>
					<DocsLink
						path={docs}
						className="mr-auto self-center text-sm text-muted-foreground"
					/>
					<Button variant="outline" disabled={checking} onClick={onRecheck}>
						{checking && <Spinner className="size-4" />}
						{m.profiles_doctor_again()}
					</Button>
					<Button onClick={onClose}>{m.common_done()}</Button>
				</DialogFooter>
			</DialogContent>
		</Dialog>
	);
};

/** Checks that **Set up Remote Desktop** can clear: the role, licensing, or a restart it asked for. */
const PREPARES = new Set(["rds_role", "rds_licensing", "restart"]);

/**
 * **Seats**: the switch, the Remote Desktop choice, and what the checks found. While seats are on,
 * changing the choice turns them on again with it. A refused turn-on answers off with its errors,
 * which is what the list shows. When Windows lacks the Session Host role or licensing, **Set up
 * Remote Desktop** installs them and turns seats on in one go. Every change asks for the console
 * password first.
 */
export const SeatsDialog: FC<{
	open: boolean;
	seating: Seating | undefined;
	isPending: boolean;
	/** The pending change is **Set up Remote Desktop**, which takes minutes. */
	preparing?: boolean;
	onChange: (
		change: { enabled: boolean; allowRdp: boolean; prepare?: boolean },
		password: string,
	) => unknown;
	/** Restart the host once the console password is re-entered. */
	onRestart?: (password: string) => Promise<void>;
	onClose: () => void;
}> = ({
	open,
	seating,
	isPending,
	preparing = false,
	onChange,
	onRestart,
	onClose,
}) => {
	const [allowRdp, setAllowRdp] = useState(false);
	const [restarting, setRestarting] = useState(false);
	const [asking, setAsking] = useState<{
		enabled: boolean;
		allowRdp: boolean;
		prepare?: boolean;
	} | null>(null);
	const stored = seating?.allow_rdp_from_network === true;
	useEffect(() => {
		if (open) setAllowRdp(stored);
	}, [open, stored]);
	const on = seating?.enabled === true;
	const checks = seating?.checks ?? [];
	const fixable =
		!on && checks.some((c) => PREPARES.has(c.code) && c.level !== "info");
	const needsRestart = checks.some((c) => c.code === "restart");
	if (asking) {
		return (
			<PasswordConfirmDialog
				open
				id="seats-password"
				title={m.profiles_seats_confirm_title()}
				body={m.profiles_seats_confirm_body()}
				submitLabel={m.common_confirm()}
				busyLabel={asking.prepare ? m.profiles_seats_preparing() : undefined}
				failedText={m.profiles_seats_failed()}
				onSubmit={async (password) => {
					await onChange(asking, password);
				}}
				onClose={() => {
					// While seats are on, the choice on screen is the stored one.
					if (on) setAllowRdp(stored);
					setAsking(null);
				}}
			/>
		);
	}
	if (restarting && onRestart) {
		return (
			<PasswordConfirmDialog
				open
				id="seats-restart-password"
				title={m.host_power_confirm_title({ action: m.host_power_reboot() })}
				body={m.host_power_confirm_body()}
				submitLabel={m.host_power_reboot()}
				busyLabel={m.host_power_working()}
				destructive
				onSubmit={async (password) => {
					await onRestart(password);
					setRestarting(false);
					onClose();
				}}
				onClose={() => setRestarting(false)}
			/>
		);
	}
	return (
		<Dialog open={open} onOpenChange={(o) => !o && onClose()}>
			<DialogContent className="max-w-md">
				<DialogHeader>
					<DialogTitle>{m.profiles_seats()}</DialogTitle>
				</DialogHeader>
				<p className="text-sm text-muted-foreground">
					{m.profiles_seats_intro()} <DocsLink path="windows-seats" />
				</p>
				<div className="flex items-center gap-2">
					<Checkbox
						id="seats-on"
						checked={on}
						disabled={isPending || !seating}
						onCheckedChange={(v) =>
							setAsking({ enabled: v === true, allowRdp })
						}
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
								if (on) setAsking({ enabled: true, allowRdp: v === true });
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
				{!seating && (
					<p className="text-sm text-muted-foreground">{m.common_loading()}</p>
				)}
				{checks.length > 0 && <CheckList checks={checks} />}
				{needsRestart && onRestart && (
					<Button
						variant="outline"
						disabled={isPending}
						onClick={() => setRestarting(true)}
					>
						{m.host_power_reboot()}
					</Button>
				)}
				{fixable && !needsRestart && (
					<div className="space-y-1">
						<Button
							variant="outline"
							disabled={isPending}
							onClick={() =>
								setAsking({ enabled: true, allowRdp, prepare: true })
							}
						>
							{preparing && <Spinner className="size-4" />}
							{preparing
								? m.profiles_seats_preparing()
								: m.profiles_seats_prepare()}
						</Button>
						<p className="text-xs text-muted-foreground">
							{m.profiles_seats_prepare_hint()}
						</p>
					</div>
				)}
				{on && !isPending && (
					<p className="text-sm">{m.profiles_seats_next()}</p>
				)}
				<DialogFooter>
					<Button onClick={onClose}>{m.common_done()}</Button>
				</DialogFooter>
			</DialogContent>
		</Dialog>
	);
};
