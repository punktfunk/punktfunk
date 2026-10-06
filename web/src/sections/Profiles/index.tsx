import { useMutation, useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { type FC, useState } from "react";
import {
	getGetHostSettingsQueryKey,
	patchHostSettings,
	useGetHostInfo,
	useGetHostSettings,
} from "@/api/gen/host/host";
import type { ProfileAdmin } from "@/api/gen/model/profileAdmin";
import type { ProfileCreate } from "@/api/gen/model/profileCreate";
import {
	deleteProfile,
	endProfileSession,
	getGetSeatingQueryKey,
	getGetSeatsDoctorQueryKey,
	getListProfilesQueryKey,
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
} from "@/api/gen/profiles/profiles";
import { useDialogs } from "@/components/dialogs";
import { usePasswordFailure } from "@/components/password-confirm";
import { apiErrorMessage } from "@/lib/errors";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";
import {
	AddProfileDialog,
	ProfilesView,
	RemoveProfileDialog,
	type SeatHome,
	SeatsDialog,
} from "./view";

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
	const host = useGetHostInfo();
	const linux = host.data?.os?.startsWith("linux") ?? false;
	const windows = host.data?.os?.startsWith("windows") ?? false;
	const seating = useGetSeating({ query: { enabled: windows } });
	const seatsOn = seating.data?.enabled === true;
	const doctor = useGetSeatsDoctor({
		query: { enabled: windows && seatsOn, refetchInterval: 60_000 },
	});
	const [seatsOpen, setSeatsOpen] = useState(false);
	const settings = useGetHostSettings({ query: { enabled: linux } });
	const seatRow = settings.data?.settings.find(
		(s) => s.id === "steam_seat_home",
	);
	// Untouched, the first Own Steam profile turns it on. A value the operator set stays.
	const seatHome: SeatHome =
		!seatRow || seatRow.value === true
			? "on"
			: seatRow.source === "default"
				? "turns-on"
				: "off";
	const ownerName = profiles.data?.find((p) => p.owner)?.display_name ?? "";
	const [adding, setAdding] = useState(false);
	const [removing, setRemoving] = useState<ProfileAdmin | null>(null);
	const [versions, setVersions] = useState<Record<string, number>>({});
	const removeFailure = usePasswordFailure();

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
	// A refused turn-on answers `enabled: false` with its checks, so only a transport failure toasts.
	const changeSeating = useSetSeating({
		mutation: {
			onSuccess: (next) => {
				qc.setQueryData(getGetSeatingQueryKey(), next);
				qc.invalidateQueries({ queryKey: getGetSeatsDoctorQueryKey() });
				refresh();
			},
			onError: failed(m.profiles_seats_failed()),
		},
	});
	const act = (p: ProfileAdmin, a: SeatAct) =>
		seatAct.mutate({ id: p.id, act: a });
	const firstError = doctor.data?.diagnostics.find((d) => d.level === "error");
	const doctorLine =
		seatsOn && (doctor.data || doctor.error)
			? {
					message: doctor.data
						? (firstError?.message ?? null)
						: (apiErrorMessage(doctor.error) ?? m.common_error()),
					checking: doctor.isFetching,
					onRun: () => void doctor.refetch(),
				}
			: null;

	const upload = (id: string, file: File) =>
		picture.mutate(
			{ id, file },
			{
				onSuccess: () => {
					setVersions((v) => ({ ...v, [id]: (v[id] ?? 0) + 1 }));
					refresh();
				},
				onError: () => toast.error(m.profiles_picture_failed()),
			},
		);

	const turnOnSeatHome = async () => {
		try {
			const next = await patchHostSettings({ steam_seat_home: true });
			qc.setQueryData(getGetHostSettingsQueryKey(), next);
		} catch (e) {
			toast.error(apiErrorMessage(e) ?? m.profiles_seat_home_failed());
		}
	};

	const onCreate = (body: ProfileCreate, file: File | null) =>
		create.mutate(
			{ data: body },
			{
				onSuccess: async (made) => {
					setAdding(false);
					if (body.seat && seatHome === "turns-on") await turnOnSeatHome();
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

	const onRemove = (id: string, erase: boolean, password: string) =>
		remove.mutate(
			{ id, erase, password },
			{
				onSuccess: () => {
					removeFailure.reset();
					setRemoving(null);
					refresh();
				},
				onError: (e) => {
					if (!removeFailure.classify(e)) failed(m.profiles_remove_failed())(e);
				},
			},
		);

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
				avatarVersions={versions}
				seatHome={seatHome}
				windows={
					windows
						? {
								onSeats: () => setSeatsOpen(true),
								onStart: (p) => act(p, "start"),
								onStop: (p) => act(p, "stop"),
								onEnd: (p) => act(p, "end"),
								doctor: doctorLine,
							}
						: undefined
				}
				onAdd={() => setAdding(true)}
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
				onRemove={(p) => {
					removeFailure.reset();
					setRemoving(p);
				}}
				busyId={busyId}
			/>
			<AddProfileDialog
				open={adding}
				ownerName={ownerName}
				linux={linux}
				windows={windows}
				seatsOn={seatsOn}
				seatHome={seatHome}
				onCancel={() => setAdding(false)}
				onCreate={onCreate}
				isPending={create.isPending}
			/>
			{windows && (
				<SeatsDialog
					open={seatsOpen}
					seating={seating.data}
					isPending={changeSeating.isPending}
					onChange={(enabled, allowRdp) =>
						changeSeating.mutate({
							data: { enabled, allow_rdp_from_network: allowRdp },
						})
					}
					onClose={() => setSeatsOpen(false)}
				/>
			)}
			<RemoveProfileDialog
				windows={windows}
				profile={removing}
				onCancel={() => setRemoving(null)}
				onRemove={onRemove}
				isPending={remove.isPending}
				failure={removeFailure.failure}
			/>
		</>
	);
};
