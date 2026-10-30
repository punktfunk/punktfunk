import { useMutation, useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { type FC, useEffect, useState } from "react";
import { apiFetch } from "@/api/fetcher";
import { useGetHostInfo } from "@/api/gen/host/host";
import type { DoorChange } from "@/api/gen/model/doorChange";
import type { ProfileAdmin } from "@/api/gen/model/profileAdmin";
import type { ProfileCreate } from "@/api/gen/model/profileCreate";
import type { SeatingChange } from "@/api/gen/model/seatingChange";
import {
	deleteProfile,
	endProfileSession,
	getGetSeatingQueryKey,
	getGetSeatsDoctorQueryKey,
	getListProfilesQueryKey,
	getWindowsAccountsQueryKey,
	setDoor,
	setProfileAvatar,
	startProfileSeat,
	stopProfileSeat,
	useCreateProfile,
	useDeleteProfileAvatar,
	useGetSeating,
	useGetSeatsDoctor,
	useListProfiles,
	useSetSeating,
	useUpdateProfile,
	useWindowsAccounts,
} from "@/api/gen/profiles/profiles";
import { useDialogs } from "@/components/dialogs";
import { apiErrorMessage } from "@/lib/errors";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";
import {
	AddProfileDialog,
	DoctorDialog,
	DoorDialog,
	ProfilesView,
	RemoveProfileDialog,
	SeatPasswordDialog,
	SeatsDialog,
} from "./view";

/** How long a door switch may take before the page says so: it restarts the host and console. */
const DOOR_SLOW_MS = 120_000;

type SeatAct = "start" | "stop" | "end";
const SEAT_ACT = {
	start: startProfileSeat,
	stop: stopProfileSeat,
	end: endProfileSession,
};

/** The Profiles page: the cards, **Add profile** and **Remove**. Edits are the operator's. */
export const SectionProfiles: FC = () => {
	useLocale();
	const qc = useQueryClient();
	const { promptText } = useDialogs();
	// A seat that is starting changes within seconds; the rest of the time the list is a glance.
	const profiles = useListProfiles({
		query: {
			refetchInterval: (q) =>
				q.state.data?.some((p) => p.seat?.state === "starting")
					? 2_000
					: 15_000,
		},
	});
	// The state a door switch was asked for, until the host that answers says it took.
	const [switching, setSwitching] = useState<boolean | null>(null);
	const host = useGetHostInfo({
		query: { refetchInterval: switching === null ? false : 2_000 },
	});
	const linux = host.data?.os?.startsWith("linux") ?? false;
	const windows = host.data?.os?.startsWith("windows") ?? false;
	const door = host.data?.door === true;
	// Seats are the box's own on Windows, and a Linux door's.
	const seatHost = windows || door;
	const seating = useGetSeating({ query: { enabled: seatHost } });
	const seatsOn = seating.data?.enabled === true;
	// Local Windows accounts no profile holds, for take turns and a seat; only Windows has any.
	const winAccounts = useWindowsAccounts({ query: { enabled: windows } });
	const doctor = useGetSeatsDoctor({
		query: { enabled: seatHost && seatsOn, refetchInterval: 60_000 },
	});
	const [doorAsk, setDoorAsk] = useState<boolean | null>(null);
	useEffect(() => {
		if (switching === null) return;
		if (host.data?.door_switch_failed) {
			toast.error(m.profiles_door_failed());
			setSwitching(null);
			return;
		}
		if (host.data?.door !== switching) return;
		setSwitching(null);
		// Every list now answers from the other host.
		void qc.invalidateQueries();
	}, [host.data?.door, host.data?.door_switch_failed, switching, qc]);
	useEffect(() => {
		if (switching === null) return;
		const slow = setTimeout(() => {
			toast.error(m.profiles_door_slow());
			setSwitching(null);
		}, DOOR_SLOW_MS);
		return () => clearTimeout(slow);
	}, [switching]);
	const [seatsOpen, setSeatsOpen] = useState(false);
	const [doctorOpen, setDoctorOpen] = useState(false);
	const ownerName = profiles.data?.find((p) => p.owner)?.display_name ?? "";
	const [adding, setAdding] = useState(false);
	const [removing, setRemoving] = useState<ProfileAdmin | null>(null);
	const [passwordFor, setPasswordFor] = useState<ProfileAdmin | null>(null);
	const refresh = () =>
		qc.invalidateQueries({ queryKey: getListProfilesQueryKey() });
	const failed = (fallback: string) => (e: unknown) =>
		toast.error(apiErrorMessage(e) ?? fallback);

	const create = useCreateProfile();
	const update = useUpdateProfile();
	const clearPicture = useDeleteProfileAvatar();
	const picture = useMutation({
		mutationFn: ({ id, file }: { id: string; file: File }) =>
			setProfileAvatar(id, file, {
				headers: { "Content-Type": file.type || "image/png" },
			}),
	});
	const remove = useMutation({
		mutationFn: (v: { id: string; erase: boolean; password: string }) =>
			deleteProfile(
				v.id,
				{ erase: v.erase },
				{
					headers: { "content-type": "application/json" },
					body: JSON.stringify({ password: v.password }),
				},
			),
	});

	// The answer is the profile as the host sees it now.
	const seatAct = useMutation({
		mutationFn: ({ id, act }: { id: string; act: SeatAct }) =>
			SEAT_ACT[act](id),
		onSuccess: (row) =>
			qc.setQueryData<ProfileAdmin[]>(getListProfilesQueryKey(), (rows) =>
				rows?.map((p) => (p.id === row.id ? row : p)),
			),
		onError: failed(m.profiles_seat_failed()),
	});
	// A refused turn-on answers `enabled: false` with its checks; the password dialog shows the rest.
	const changeSeating = useSetSeating({
		mutation: {
			onSuccess: (next) => {
				qc.setQueryData(getGetSeatingQueryKey(), next);
				qc.invalidateQueries({ queryKey: getGetSeatsDoctorQueryKey() });
				refresh();
			},
		},
	});
	// The console password rides in the body; the console's server strips it before the host.
	const changeDoor = useMutation({
		mutationFn: (v: { on: boolean; password: string }) =>
			setDoor({ on: v.on, password: v.password } as DoorChange),
		onSuccess: (_done, v) => setSwitching(v.on),
	});
	const act = (p: ProfileAdmin, a: SeatAct) =>
		seatAct.mutate({ id: p.id, act: a });
	const firstError = doctor.data?.diagnostics.find((d) => d.level === "error");
	const doctorError = doctor.error
		? (apiErrorMessage(doctor.error) ?? m.common_error())
		: null;
	const doctorLine =
		seatsOn && (doctor.data || doctor.error)
			? {
					message: doctor.data ? (firstError?.message ?? null) : doctorError,
					notes:
						doctor.data?.diagnostics.filter((d) => d.level === "warning")
							.length ?? 0,
					onOpen: () => {
						setDoctorOpen(true);
						void doctor.refetch();
					},
				}
			: null;

	const upload = (id: string, file: File) =>
		picture.mutate(
			{ id, file },
			{
				onSuccess: refresh,
				onError: () => toast.error(m.profiles_picture_failed()),
			},
		);

	const onCreate = (body: ProfileCreate, file: File | null) =>
		create.mutate(
			{ data: body },
			{
				onSuccess: (made) => {
					setAdding(false);
					if (file) upload(made.id, file);
					else refresh();
				},
				onError: failed(m.profiles_add_failed()),
			},
		);

	const onRename = async (p: ProfileAdmin) => {
		const next = await promptText({
			title: m.profiles_rename_title(),
			label: m.profiles_name(),
			defaultValue: p.display_name,
			confirmLabel: m.action_rename(),
		});
		if (!next?.trim() || next.trim() === p.display_name) return;
		update.mutate(
			{ id: p.id, data: { display_name: next.trim() } },
			{ onSuccess: refresh, onError: failed(m.profiles_rename_failed()) },
		);
	};

	const onRemove = async (id: string, erase: boolean, password: string) => {
		await remove.mutateAsync({ id, erase, password });
		refresh();
	};

	const busyId =
		(update.isPending ? update.variables?.id : undefined) ??
		(picture.isPending ? picture.variables?.id : undefined) ??
		(clearPicture.isPending ? clearPicture.variables?.id : undefined) ??
		(seatAct.isPending ? seatAct.variables?.id : undefined) ??
		null;

	return (
		<>
			<ProfilesView
				profiles={profiles}
				seats={
					seatHost
						? {
								onSeats: windows ? () => setSeatsOpen(true) : undefined,
								onStart: (p) => act(p, "start"),
								onStop: (p) => act(p, "stop"),
								onEnd: (p) => act(p, "end"),
								onPassword: setPasswordFor,
								doctor: doctorLine,
							}
						: undefined
				}
				door={
					linux
						? {
								on: door,
								changing: switching !== null,
								unavailable: host.data?.door_unavailable,
								onChange: () => setDoorAsk(!door),
							}
						: undefined
				}
				onAdd={() => {
					// The pickers list what is free now: a profile added or removed since changes it.
					void qc.invalidateQueries({ queryKey: getWindowsAccountsQueryKey() });
					setAdding(true);
				}}
				onRename={onRename}
				onPicture={(p, file) => upload(p.id, file)}
				onRemovePicture={(p) =>
					clearPicture.mutate(
						{ id: p.id },
						{
							onSuccess: refresh,
							onError: failed(m.profiles_picture_failed()),
						},
					)
				}
				onRemove={setRemoving}
				busyId={busyId}
			/>
			<AddProfileDialog
				open={adding}
				ownerName={ownerName}
				linux={linux}
				windows={windows}
				seatsOn={seatsOn}
				door={door}
				needsServer={
					seating.data?.checks.some(
						(c) => c.code === "windows_server" && c.level === "error",
					) === true
				}
				windowsAccounts={winAccounts.data ?? []}
				takenAccents={profiles.data?.map((p) => p.accent ?? "") ?? []}
				onCancel={() => setAdding(false)}
				onCreate={onCreate}
				isPending={create.isPending}
			/>
			{windows && (
				<SeatsDialog
					open={seatsOpen}
					seating={seating.data}
					isPending={changeSeating.isPending}
					preparing={
						changeSeating.isPending &&
						changeSeating.variables?.data.prepare === true
					}
					onChange={(change, password) =>
						changeSeating.mutateAsync({
							data: {
								enabled: change.enabled,
								allow_rdp_from_network: change.allowRdp,
								prepare: change.prepare,
								password,
							} as SeatingChange,
						})
					}
					onRestart={async (password) => {
						await apiFetch("/api/v1/actions/power.reboot", {
							method: "POST",
							headers: { "Content-Type": "application/json" },
							body: JSON.stringify({ password }),
						});
					}}
					onClose={() => setSeatsOpen(false)}
				/>
			)}
			{seatHost && (
				<DoctorDialog
					open={doctorOpen}
					doctor={doctor.data}
					error={doctorError}
					checking={doctor.isFetching}
					docs={windows ? "windows-seats#troubleshooting" : "profiles#on-linux"}
					onRecheck={() => void doctor.refetch()}
					onClose={() => setDoctorOpen(false)}
				/>
			)}
			<DoorDialog
				open={doorAsk !== null}
				turningOn={doorAsk === true}
				onConfirm={async (password) => {
					await changeDoor.mutateAsync({ on: doorAsk === true, password });
				}}
				onCancel={() => setDoorAsk(null)}
			/>
			<RemoveProfileDialog
				windows={windows}
				profile={removing}
				onCancel={() => setRemoving(null)}
				onRemove={onRemove}
			/>
			<SeatPasswordDialog
				profile={passwordFor}
				onUpdate={async (id, password) => {
					await update.mutateAsync({ id, data: { seat_password: password } });
					refresh();
				}}
				onClose={() => setPasswordFor(null)}
			/>
		</>
	);
};
