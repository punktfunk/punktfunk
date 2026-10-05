import { useMutation, useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { type FC, useState } from "react";
import { useGetHostInfo } from "@/api/gen/host/host";
import type { ProfileAdmin } from "@/api/gen/model/profileAdmin";
import type { ProfileCreate } from "@/api/gen/model/profileCreate";
import {
	deleteProfile,
	getListProfilesQueryKey,
	setProfileAvatar,
	useCreateProfile,
	useDeleteProfileAvatar,
	useListProfiles,
	useUpdateProfile,
} from "@/api/gen/profiles/profiles";
import { useDialogs } from "@/components/dialogs";
import { usePasswordFailure } from "@/components/password-confirm";
import { apiErrorMessage } from "@/lib/errors";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";
import { AddProfileDialog, ProfilesView, RemoveProfileDialog } from "./view";

/** The Profiles page: the cards, **Add profile** and **Remove**. Edits are the operator's. */
export const SectionProfiles: FC = () => {
	useLocale();
	const qc = useQueryClient();
	const { promptText } = useDialogs();
	const profiles = useListProfiles({ query: { refetchInterval: 15_000 } });
	const host = useGetHostInfo();
	const linux = host.data?.os?.startsWith("linux") ?? false;
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
		null;

	return (
		<>
			<ProfilesView
				profiles={profiles}
				avatarVersions={versions}
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
				onCancel={() => setAdding(false)}
				onCreate={onCreate}
				isPending={create.isPending}
			/>
			<RemoveProfileDialog
				profile={removing}
				onCancel={() => setRemoving(null)}
				onRemove={onRemove}
				isPending={remove.isPending}
				failure={removeFailure.failure}
			/>
		</>
	);
};
