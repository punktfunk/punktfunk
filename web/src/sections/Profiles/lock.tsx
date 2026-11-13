// A profile's lock on the Profiles page: its part of the card, the dialog that locks it, and the
// dialog that manages a locked one. The box host keeps every lock. A profile that shares the
// owner's desktop opens with the owner's lock; one on a person's own account has none.
import { type QueryClient, useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { Lock, TriangleAlert } from "lucide-react";
import { type FC, type ReactNode, useState } from "react";
import { useListPairedClients } from "@/api/gen/clients/clients";
import type { LockProfile } from "@/api/gen/model/lockProfile";
import type { PlayingDevice } from "@/api/gen/model/playingDevice";
import type { ProfileAdmin } from "@/api/gen/model/profileAdmin";
import type { ProfileDeviceAdd } from "@/api/gen/model/profileDeviceAdd";
import type { ProfileLock } from "@/api/gen/model/profileLock";
import type { ProfilePin } from "@/api/gen/model/profilePin";
import type { SignedInDevice } from "@/api/gen/model/signedInDevice";
import { useListNativeClients } from "@/api/gen/native/native";
import {
	addProfileDevice,
	clearProfilePin,
	getGetProfileLockQueryKey,
	getListProfileDevicesQueryKey,
	getListProfilesQueryKey,
	lockProfile,
	setProfilePin,
	unlockProfile,
	useGetProfileLock,
	useListProfileDevices,
} from "@/api/gen/profiles/profiles";
import { type ConfirmOptions, useDialogs } from "@/components/dialogs";
import {
	PasswordConfirmDialog,
	PasswordConfirmField,
	usePasswordFailure,
} from "@/components/password-confirm";
import { badgeVariants } from "@/components/ui/badge";
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
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { Spinner } from "@/components/ui/spinner";
import { apiErrorMessage } from "@/lib/errors";
import { digitsOnly } from "@/lib/ime";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";
import { deviceName, pairedListView } from "@/sections/Devices/pairedRows";
import { DevicesList, signOut } from "./devices";

/** A PIN the host takes: 4 to 8 ASCII digits. */
export const validPin = (pin: string): boolean => /^[0-9]{4,8}$/.test(pin);

/** What a card shows of the lock its profile opens with, as the profile list reports it. */
export type LockCard = {
	locked: boolean;
	/** The profile whose lock it is: its own, or the owner's for a sharer of the desktop. */
	holder: string;
	/** The owner's name, when it opens with the owner's lock. */
	owner?: string;
	/** Wrong PINs in the last 15 minutes, on a lock of its own. */
	failedPins?: number;
	/** Opens the dialog that locks it, or the one that manages it when locked. */
	onOpen?: () => void;
};

/** What `p`'s card shows of its lock: `undefined` on a person's own account, which has none. */
export function lockCard(
	p: ProfileAdmin,
	list: readonly ProfileAdmin[],
): LockCard | undefined {
	const lock = p.lock;
	if (!lock) return undefined;
	const holder = lock.shared_with ?? p.id;
	if (lock.shared_with) {
		const owner = list.find((o) => o.id === holder)?.display_name ?? holder;
		return { locked: lock.locked, holder, owner };
	}
	return { locked: lock.locked, holder, failedPins: lock.failed_pins };
}

/** The status beside the name: **Locked**, which opens the lock's dialog. */
export const LockChip: FC<{ lock: LockCard }> = ({ lock }) => {
	if (!lock.locked) return null;
	const look = cn(badgeVariants({ variant: "outline" }), "gap-1");
	const body = (
		<>
			<Lock className="size-3" aria-hidden />
			{m.profiles_locked()}
		</>
	);
	return lock.onOpen ? (
		<button
			type="button"
			aria-haspopup="dialog"
			onClick={lock.onOpen}
			className={cn(
				look,
				"cursor-pointer hover:bg-muted focus-visible:ring-2 focus-visible:ring-ring",
			)}
		>
			{body}
		</button>
	) : (
		<span className={look}>{body}</span>
	);
};

/** The line under a locked card's facts: whose lock it uses, or its wrong PINs. */
export const LockLines: FC<{ lock: LockCard }> = ({ lock }) => {
	if (!lock.locked) return null;
	if (lock.owner) {
		return (
			<p className="text-sm text-muted-foreground">
				{m.profiles_lock_shared_line({ owner: lock.owner })}
			</p>
		);
	}
	return lock.failedPins ? (
		<p className="text-sm text-muted-foreground">
			{m.profiles_lock_failed_pins({ count: lock.failedPins })}
		</p>
	) : null;
};

/** The card's one action on an open profile: lock it, or the owner's desktop it shares. */
export const LockAction: FC<{ lock: LockCard; disabled?: boolean }> = ({
	lock,
	disabled,
}) => {
	if (lock.locked || !lock.onOpen) return null;
	const label = lock.owner
		? m.profiles_lock_owner_action({ owner: lock.owner })
		: m.profiles_lock_action();
	return (
		<Button
			variant="ghost"
			size="icon"
			aria-label={label}
			aria-haspopup="dialog"
			title={label}
			disabled={disabled}
			onClick={lock.onOpen}
		>
			<Lock className="size-4" />
		</Button>
	);
};

/**
 * Locks profile `id` with exactly `devices`, in one step that changes nothing when refused. A
 * PIN rides along with the console password; without one the body carries neither, and the
 * console asks for none.
 */
export const lockWith = (
	id: string,
	v: { devices: string[]; pin: string; password: string },
): Promise<ProfileLock> =>
	lockProfile(
		id,
		(v.pin
			? { devices: v.devices, pin: v.pin, password: v.password }
			: { devices: v.devices }) as LockProfile,
	);

/**
 * Writes `next`, the host's answer for `holder`'s lock, into the profile list at once: the
 * holder's row and every row that shares its lock. The list is read again behind it.
 */
function showLock(qc: QueryClient, holder: string, next: ProfileLock) {
	const { playing: _, ...summary } = next;
	qc.setQueryData<ProfileAdmin[]>(getListProfilesQueryKey(), (rows) =>
		rows?.map((p) =>
			p.id === holder
				? { ...p, lock: { ...summary, shared_with: null } }
				: p.lock?.shared_with === holder
					? { ...p, lock: { ...summary, shared_with: holder } }
					: p,
		),
	);
	void qc.invalidateQueries({ queryKey: getListProfilesQueryKey() });
}

/** The page's part: what a card's lock action opens, and the dialogs. */
export function useProfileLocks(list: ProfileAdmin[]) {
	const [open, setOpen] = useState<{ id: string; locked: boolean } | null>(
		null,
	);
	const target = open && list.find((p) => p.id === open.id);
	const dialogs = target ? (
		<LockDialogs
			key={target.id}
			profile={target}
			locked={open.locked}
			sharers={list.some((p) => p.lock?.shared_with === target.id)}
			onClose={() => setOpen(null)}
		/>
	) : null;
	return {
		onLock: (holder: string, locked: boolean) =>
			setOpen({ id: holder, locked }),
		dialogs,
	};
}

/** A device the lock dialog offers. */
export type LockRow = {
	fingerprint: string;
	name: string;
	moonlight: boolean;
};

/** The paired devices, then a playing one the lists don't have yet. Only these can be kept. */
function lockRows(paired: LockRow[], playing: PlayingDevice[]): LockRow[] {
	const rows = [...paired];
	for (const d of playing) {
		const fp = d.fingerprint?.toLowerCase();
		if (!fp || rows.some((r) => r.fingerprint === fp)) continue;
		rows.push({ fingerprint: fp, name: d.name, moonlight: false });
	}
	return rows;
}

/** The question before `device`, or every device for `null`, is signed out of locked `name`. */
export const signOutAsk = (
	name: string,
	pin: boolean,
	device: SignedInDevice | null,
): ConfirmOptions => ({
	title: device
		? m.profiles_device_sign_out_title({ device: deviceName(device) })
		: m.profiles_devices_sign_out_all_title(),
	description: (device
		? pin
			? m.profiles_lock_sign_out_body_pin
			: m.profiles_lock_sign_out_body
		: pin
			? m.profiles_lock_sign_out_all_body_pin
			: m.profiles_lock_sign_out_all_body)({ name }),
	confirmLabel: device
		? m.profiles_device_sign_out()
		: m.profiles_devices_sign_out_all(),
	destructive: true,
});

/** The dialog for one profile's lock: the lock flow while it's open, else its management. */
const LockDialogs: FC<{
	profile: ProfileAdmin;
	locked: boolean;
	/** The owner's profile, which other profiles share. */
	sharers: boolean;
	onClose: () => void;
}> = ({ profile: p, locked, sharers, onClose }) => {
	const qc = useQueryClient();
	const { confirm } = useDialogs();
	const key = getGetProfileLockQueryKey(p.id);
	const lock = useGetProfileLock(p.id, { query: { refetchOnMount: "always" } });
	const devices = useListProfileDevices(p.id, {
		query: { refetchInterval: 15_000 },
	});
	const native = useListNativeClients();
	const moonlight = useListPairedClients();
	const paired = pairedListView(native, moonlight);
	const pairedRows: LockRow[] = paired.rows.map((r) => ({
		fingerprint: r.fingerprint.toLowerCase(),
		name: deviceName(r),
		moonlight: r.protocol === "moonlight",
	}));
	// A list that didn't load hides devices the lock would cut off: the lock waits for it.
	const loadError =
		paired.nativeError ?? paired.moonlightError ?? devices.error;
	const set = (next: ProfileLock) => {
		qc.setQueryData(key, next);
		showLock(qc, p.id, next);
	};
	const reread = () =>
		qc.invalidateQueries({ queryKey: getListProfileDevicesQueryKey(p.id) });
	const name = p.display_name;
	const listed = devices.data ?? [];
	const removePin = async () => {
		const ok = await confirm({
			title: m.profiles_pin_remove_title({ name }),
			description: m.profiles_pin_remove_body({ name }),
			confirmLabel: m.profiles_pin_remove(),
			destructive: true,
		});
		if (!ok) return;
		try {
			set(await clearProfilePin(p.id));
		} catch (e) {
			toast.error(apiErrorMessage(e) ?? m.profiles_pin_remove_failed());
		}
	};

	if (!locked) {
		const ready = !paired.loading && !devices.isLoading && !!lock.data;
		const playing = lock.data?.playing ?? [];
		return (
			<LockFlowDialog
				name={name}
				owner={p.owner}
				sharers={sharers}
				rows={ready ? lockRows(pairedRows, playing) : undefined}
				// What plays now, and what a past lock let in: chosen until unchosen.
				kept={[
					...playing.flatMap((d) => (d.fingerprint ? [d.fingerprint] : [])),
					...listed.map((d) => d.fingerprint),
				]}
				playing={playing}
				hasPin={lock.data?.pin === true}
				error={
					loadError ? (apiErrorMessage(loadError) ?? m.common_error()) : null
				}
				onRetry={() => {
					void native.refetch();
					void moonlight.refetch();
					void devices.refetch();
				}}
				onRemovePin={removePin}
				onLock={async (v) => {
					set(await lockWith(p.id, v));
					void reread();
					onClose();
				}}
				onClose={onClose}
			/>
		);
	}

	const pin = lock.data?.pin === true;
	const ask = async (device: SignedInDevice | null) => {
		if (!(await confirm(signOutAsk(name, pin, device)))) return;
		try {
			await signOut(qc, p.id, device?.fingerprint ?? null);
		} catch (e) {
			toast.error(apiErrorMessage(e) ?? m.profiles_devices_failed());
		}
	};
	const onList = new Set(listed.map((d) => d.fingerprint.toLowerCase()));
	return (
		<LockManageDialog
			name={name}
			lock={lock.data}
			lockError={
				lock.error ? (apiErrorMessage(lock.error) ?? m.common_error()) : null
			}
			devices={devices.data}
			devicesError={
				devices.error
					? (apiErrorMessage(devices.error) ?? m.common_error())
					: null
			}
			onRetry={() => {
				void lock.refetch();
				void devices.refetch();
			}}
			candidates={
				paired.loading
					? undefined
					: pairedRows.filter((r) => !onList.has(r.fingerprint))
			}
			onSignOut={ask}
			onSignOutAll={() => ask(null)}
			onAdd={async (fingerprint, password) => {
				const next = await addProfileDevice(p.id, {
					fingerprint,
					password,
				} as ProfileDeviceAdd);
				qc.setQueryData(getListProfileDevicesQueryKey(p.id), next);
			}}
			onSetPin={async (next, password) => {
				set(await setProfilePin(p.id, { pin: next, password } as ProfilePin));
			}}
			onRemovePin={removePin}
			onUnlock={async (password) => {
				set(
					await unlockProfile(p.id, {
						headers: { "content-type": "application/json" },
						body: JSON.stringify({ password }),
					}),
				);
				onClose();
			}}
			onClose={onClose}
		/>
	);
};

/**
 * A PIN field: digits only, up to 8, never prefilled. Once left with a value the host would
 * refuse, it says why in the host's words.
 */
export const PinField: FC<{
	id: string;
	label: string;
	hint: string;
	value: string;
	onChange: (pin: string) => void;
	autoFocus?: boolean;
}> = ({ id, label, hint, value, onChange, autoFocus }) => {
	const [left, setLeft] = useState(false);
	const refused = left && value !== "" && !validPin(value);
	return (
		<div className="space-y-2">
			<Label htmlFor={id}>{label}</Label>
			<Input
				id={id}
				inputMode="numeric"
				autoComplete="off"
				data-1p-ignore
				spellCheck={false}
				maxLength={8}
				autoFocus={autoFocus}
				aria-invalid={refused || undefined}
				aria-describedby={`${id}-hint`}
				className="max-w-40 font-mono tabular-nums"
				value={value}
				onChange={(e) => onChange(digitsOnly(e.target.value).slice(0, 8))}
				onBlur={() => setLeft(true)}
			/>
			<p id={`${id}-hint`} className="text-xs text-muted-foreground">
				{hint}
			</p>
			{refused && (
				<p role="alert" className="text-xs text-destructive">
					{m.apierr_profile_pin_invalid()}
				</p>
			)}
		</div>
	);
};

/** A line with a warning sign: what a choice will do to someone. */
const Warning: FC<{ children: ReactNode }> = ({ children }) => (
	<p className="flex items-start gap-2 text-sm">
		<TriangleAlert
			className="mt-0.5 size-4 shrink-0 text-[var(--warning)]"
			aria-hidden
		/>
		<span>{children}</span>
	</p>
);

/**
 * **Lock**: what locking does, the paired devices to keep with the ones playing and the ones a
 * past lock kept already chosen, and an optional PIN. A device playing now that isn't chosen,
 * or that matches no paired device and can't be, is named before the lock. A PIN asks for the
 * console password; the lock alone doesn't.
 */
export const LockFlowDialog: FC<{
	name: string;
	/** The owner's profile: the host's own desktop. */
	owner: boolean;
	/** Other profiles share this desktop. */
	sharers: boolean;
	/** `undefined` while the devices load. */
	rows: LockRow[] | undefined;
	/** Fingerprints chosen until the operator says otherwise. */
	kept: string[];
	playing: PlayingDevice[];
	/** A PIN a past lock left, which this lock keeps. */
	hasPin: boolean;
	/** The host's sentence when a list of devices didn't load. The lock waits for it. */
	error: string | null;
	onRetry?: () => void;
	onRemovePin: () => void;
	onLock: (v: {
		devices: string[];
		pin: string;
		password: string;
	}) => Promise<void>;
	onClose: () => void;
}> = ({
	name,
	owner,
	sharers,
	rows,
	kept,
	playing,
	hasPin,
	error,
	onRetry,
	onRemovePin,
	onLock,
	onClose,
}) => {
	// The operator's own choices; every other row shows what `kept` says.
	const [picked, setPicked] = useState<Record<string, boolean>>({});
	const [pin, setPin] = useState("");
	const [password, setPassword] = useState("");
	const [busy, setBusy] = useState(false);
	const [failed, setFailed] = useState<string | null>(null);
	const refusal = usePasswordFailure();
	const keep = new Set(kept.map((f) => f.toLowerCase()));
	const chosen = (fp: string) => picked[fp] ?? keep.has(fp);
	const devices = (rows ?? []).map((r) => r.fingerprint).filter(chosen);
	const cutOff = playing.filter(
		(d) => !d.fingerprint || !chosen(d.fingerprint.toLowerCase()),
	);
	const wantsPin = !hasPin && pin !== "";
	const ready =
		rows !== undefined &&
		error === null &&
		!busy &&
		(!wantsPin || (validPin(pin) && password !== ""));
	const submit = async () => {
		if (!ready) return;
		setBusy(true);
		setFailed(null);
		refusal.reset();
		try {
			await onLock({ devices, pin: wantsPin ? pin : "", password });
		} catch (e) {
			if (!refusal.classify(e))
				setFailed(apiErrorMessage(e) ?? m.profiles_lock_failed());
		} finally {
			setBusy(false);
		}
	};
	return (
		<Dialog open onOpenChange={(o) => !o && !busy && onClose()}>
			<DialogContent className="max-w-md">
				<DialogHeader>
					<DialogTitle>{m.profiles_lock_title({ name })}</DialogTitle>
					<DialogDescription>
						{m.profiles_lock_body({ name })}
					</DialogDescription>
				</DialogHeader>
				{owner && (
					<div className="space-y-1 text-sm text-muted-foreground">
						<p>{m.profiles_lock_owner_body()}</p>
						{sharers && <p>{m.profiles_lock_owner_shared()}</p>}
					</div>
				)}
				<form
					id="profile-lock"
					className="space-y-4"
					onSubmit={(e) => {
						e.preventDefault();
						void submit();
					}}
				>
					<fieldset className="space-y-2">
						<legend className="text-sm font-medium">
							{m.profiles_lock_keep()}
						</legend>
						{rows === undefined ? (
							<span
								role="status"
								className="flex items-center gap-2 text-sm text-muted-foreground"
							>
								<Spinner className="size-4" />
								{m.common_loading()}
							</span>
						) : rows.length === 0 ? (
							<p className="text-sm text-muted-foreground">
								{m.pairing_native_empty()}
							</p>
						) : (
							<ul className="divide-y">
								{rows.map((r) => {
									const id = `lock-device-${r.fingerprint.slice(0, 16)}`;
									const live = playing.some(
										(d) => d.fingerprint?.toLowerCase() === r.fingerprint,
									);
									const detail = [
										live && m.devices_streaming(),
										r.moonlight && m.pairing_protocol_moonlight(),
									].filter(Boolean);
									return (
										<li
											key={r.fingerprint}
											className="flex items-start gap-3 py-2"
										>
											{/* The field border is too faint for an empty box that cuts a device off. */}
											<Checkbox
												id={id}
												className="mt-0.5 border-muted-foreground"
												checked={chosen(r.fingerprint)}
												disabled={busy}
												onCheckedChange={(v) =>
													setPicked((was) => ({
														...was,
														[r.fingerprint]: v === true,
													}))
												}
											/>
											<Label
												htmlFor={id}
												className="flex min-w-0 flex-col items-start gap-0.5 font-normal"
											>
												<span className="truncate text-sm">{r.name}</span>
												{detail.length > 0 && (
													<span className="text-xs text-muted-foreground">
														{detail.join(" · ")}
													</span>
												)}
											</Label>
										</li>
									);
								})}
							</ul>
						)}
						{error && (
							<div role="alert" className="flex flex-wrap items-center gap-x-3">
								<p className="text-sm text-destructive">{error}</p>
								{onRetry && (
									<Button
										type="button"
										variant="ghost"
										size="sm"
										onClick={onRetry}
									>
										{m.common_retry()}
									</Button>
								)}
							</div>
						)}
					</fieldset>
					<div aria-live="polite" className="space-y-1">
						{cutOff.map((d, i) => (
							<Warning key={d.fingerprint ?? `${d.name}-${i}`}>
								{m.profiles_lock_cut_off({
									device: deviceName({
										name: d.name,
										fingerprint: d.fingerprint ?? "",
									}),
								})}
							</Warning>
						))}
						{rows !== undefined &&
							devices.length === 0 &&
							!hasPin &&
							pin === "" && (
								<Warning>{m.profiles_lock_nobody({ name })}</Warning>
							)}
					</div>
					{hasPin ? (
						<div className="flex flex-wrap items-center justify-between gap-2">
							<p className="text-sm">{m.profiles_pin_on({ name })}</p>
							<Button
								type="button"
								variant="outline"
								size="sm"
								disabled={busy}
								onClick={onRemovePin}
							>
								{m.profiles_pin_remove()}
							</Button>
						</div>
					) : (
						<PinField
							id="profile-lock-pin"
							label={m.profiles_pin_optional()}
							hint={m.profiles_pin_hint({ name })}
							value={pin}
							onChange={setPin}
						/>
					)}
					{wantsPin && (
						<PasswordConfirmField
							id="profile-lock-password"
							value={password}
							onChange={setPassword}
							failure={refusal.failure}
						/>
					)}
					{failed && <p className="text-sm text-destructive">{failed}</p>}
				</form>
				<DialogFooter>
					<Button
						type="button"
						variant="outline"
						disabled={busy}
						onClick={onClose}
					>
						{m.common_cancel()}
					</Button>
					<Button
						type="submit"
						form="profile-lock"
						variant={cutOff.length > 0 ? "destructive" : "default"}
						disabled={!ready}
					>
						{busy && <Spinner className="size-4" />}
						{m.profiles_lock_submit()}
					</Button>
				</DialogFooter>
			</DialogContent>
		</Dialog>
	);
};

type ManageStep = "add" | "pin" | "unlock";

/**
 * A locked profile's dialog: who opens it, each device with how it is signed in and a sign-out,
 * **Add a device**, its PIN, and **Unlock**. The steps that let more in ask for the console
 * password in their own dialog, which takes this one's place until it closes.
 */
export const LockManageDialog: FC<{
	name: string;
	/** `undefined` while it loads. */
	lock: ProfileLock | undefined;
	lockError: string | null;
	devices: SignedInDevice[] | undefined;
	devicesError: string | null;
	onRetry: () => void;
	/** Paired devices not on its list; `undefined` while they load. */
	candidates: LockRow[] | undefined;
	onSignOut: (d: SignedInDevice) => void;
	onSignOutAll: () => void;
	onAdd: (fingerprint: string, password: string) => Promise<void>;
	onSetPin: (pin: string, password: string) => Promise<void>;
	onRemovePin: () => void;
	onUnlock: (password: string) => Promise<void>;
	onClose: () => void;
	/** Opens on a step, for a story. */
	step?: ManageStep;
}> = ({
	name,
	lock,
	lockError,
	devices,
	devicesError,
	onRetry,
	candidates,
	onSignOut,
	onSignOutAll,
	onAdd,
	onSetPin,
	onRemovePin,
	onUnlock,
	onClose,
	step: initial,
}) => {
	const [step, setStep] = useState<ManageStep | null>(initial ?? null);
	const back = () => setStep(null);
	const pin = lock?.pin === true;
	if (step === "add") {
		return (
			<AddDeviceDialog
				name={name}
				candidates={candidates ?? []}
				onAdd={onAdd}
				onClose={back}
			/>
		);
	}
	if (step === "pin") {
		return (
			<PinDialog name={name} change={pin} onSet={onSetPin} onClose={back} />
		);
	}
	if (step === "unlock") {
		return (
			<PasswordConfirmDialog
				open
				id="profile-unlock-password"
				className="max-w-md"
				title={m.profiles_unlock_title({ name })}
				body={m.profiles_unlock_body({ name })}
				submitLabel={m.profiles_unlock()}
				failedText={m.profiles_unlock_failed()}
				onSubmit={onUnlock}
				onClose={back}
			/>
		);
	}
	const failure = lockError ?? devicesError;
	return (
		<Dialog open onOpenChange={(o) => !o && onClose()}>
			<DialogContent className="max-w-lg">
				<DialogHeader>
					<DialogTitle className="flex items-center gap-2">
						<Lock className="size-4" aria-hidden />
						{m.profiles_lock_manage_title({ name })}
					</DialogTitle>
					<DialogDescription>
						{(pin
							? m.profiles_lock_manage_body_pin
							: m.profiles_lock_manage_body)({ name })}
					</DialogDescription>
				</DialogHeader>
				{failure ? (
					<div role="alert" className="space-y-2">
						<p className="text-sm text-destructive">{failure}</p>
						<Button variant="outline" size="sm" onClick={onRetry}>
							{m.common_retry()}
						</Button>
					</div>
				) : !lock || !devices ? (
					<span
						role="status"
						className="flex items-center gap-2 text-sm text-muted-foreground"
					>
						<Spinner className="size-4" />
						{m.common_loading()}
					</span>
				) : (
					<div className="space-y-6">
						<DevicesList
							flush
							devices={devices}
							error={null}
							empty={(pin
								? m.profiles_lock_devices_none_pin
								: m.profiles_lock_devices_none)({ name })}
							action={
								candidates && candidates.length > 0 ? (
									<Button
										variant="outline"
										size="sm"
										aria-haspopup="dialog"
										onClick={() => setStep("add")}
									>
										{m.profiles_lock_add()}
									</Button>
								) : undefined
							}
							onSignOut={onSignOut}
							onSignOutAll={onSignOutAll}
						/>
						{candidates?.length === 0 && devices.length === 0 && (
							<p className="text-sm text-muted-foreground">
								{m.pairing_native_empty()}
							</p>
						)}
						<section className="space-y-2">
							<h3 className="text-sm font-medium">{m.profiles_pin_label()}</h3>
							<div className="flex flex-wrap items-center justify-between gap-2">
								<p className="text-sm text-muted-foreground">
									{(pin ? m.profiles_pin_on : m.profiles_pin_off)({ name })}
								</p>
								<div className="flex flex-wrap gap-2">
									<Button
										variant="outline"
										size="sm"
										aria-haspopup="dialog"
										onClick={() => setStep("pin")}
									>
										{pin ? m.profiles_pin_change() : m.profiles_pin_set()}
									</Button>
									{pin && (
										<Button variant="ghost" size="sm" onClick={onRemovePin}>
											{m.profiles_pin_remove()}
										</Button>
									)}
								</div>
							</div>
							{lock.failed_pins > 0 && (
								<p className="text-sm text-muted-foreground">
									{m.profiles_lock_failed_pins({ count: lock.failed_pins })}
								</p>
							)}
						</section>
					</div>
				)}
				<DialogFooter>
					{lock && (
						<Button
							variant="outline"
							className="sm:mr-auto"
							aria-haspopup="dialog"
							onClick={() => setStep("unlock")}
						>
							{m.profiles_unlock()}
						</Button>
					)}
					<Button onClick={onClose}>{m.common_done()}</Button>
				</DialogFooter>
			</DialogContent>
		</Dialog>
	);
};

/** **Add a device**: one paired device that isn't on the list, behind the console password. */
export const AddDeviceDialog: FC<{
	name: string;
	candidates: LockRow[];
	onAdd: (fingerprint: string, password: string) => Promise<void>;
	onClose: () => void;
	/** Preselected, for a story. */
	initial?: string;
}> = ({ name, candidates, onAdd, onClose, initial = "" }) => {
	const [device, setDevice] = useState(initial);
	return (
		<PasswordConfirmDialog
			open
			id="profile-lock-add-password"
			className="max-w-md"
			title={m.profiles_lock_add_title({ name })}
			body={m.profiles_lock_add_body({ name })}
			submitLabel={m.common_add()}
			failedText={m.profiles_lock_add_failed()}
			ready={device !== ""}
			autoFocus={false}
			onSubmit={(password) => onAdd(device, password)}
			onClose={onClose}
		>
			<div className="space-y-2">
				<Label htmlFor="profile-lock-add">{m.profiles_lock_add_which()}</Label>
				<Select value={device} onValueChange={setDevice}>
					<SelectTrigger
						id="profile-lock-add"
						aria-label={m.profiles_lock_add_which()}
					>
						<SelectValue placeholder={m.profiles_lock_add_pick()} />
					</SelectTrigger>
					<SelectContent>
						{candidates.map((r) => (
							<SelectItem key={r.fingerprint} value={r.fingerprint}>
								{r.moonlight
									? `${r.name} · ${m.pairing_protocol_moonlight()}`
									: r.name}
							</SelectItem>
						))}
					</SelectContent>
				</Select>
			</div>
		</PasswordConfirmDialog>
	);
};

/** **Set PIN** or **Change PIN**, behind the console password. */
export const PinDialog: FC<{
	name: string;
	/** It has a PIN, which this replaces. */
	change: boolean;
	onSet: (pin: string, password: string) => Promise<void>;
	onClose: () => void;
	/** Typed already, for a story. */
	initial?: string;
}> = ({ name, change, onSet, onClose, initial = "" }) => {
	const [pin, setPin] = useState(initial);
	return (
		<PasswordConfirmDialog
			open
			id="profile-pin-password"
			className="max-w-md"
			title={(change ? m.profiles_pin_change_title : m.profiles_pin_set_title)({
				name,
			})}
			body={(change ? m.profiles_pin_change_body : m.profiles_pin_set_body)({
				name,
			})}
			submitLabel={change ? m.profiles_pin_change() : m.profiles_pin_set()}
			failedText={m.profiles_pin_failed()}
			ready={validPin(pin)}
			autoFocus={false}
			onSubmit={(password) => onSet(pin, password)}
			onClose={onClose}
		>
			<PinField
				id="profile-pin"
				label={m.profiles_pin_label()}
				hint={m.profiles_pin_hint({ name })}
				value={pin}
				onChange={setPin}
				autoFocus
			/>
		</PasswordConfirmDialog>
	);
};
