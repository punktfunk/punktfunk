// The devices signed in to a profile on a person's own Linux account, and signing them out. The
// list is the seat supervisor's; the console only reads it and takes devices off it.
import { type QueryClient, useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import type { FC } from "react";
import type { ProfileAdmin } from "@/api/gen/model/profileAdmin";
import type { SignedInDevice } from "@/api/gen/model/signedInDevice";
import type { SignedInKind } from "@/api/gen/model/signedInKind";
import {
	getListProfileDevicesQueryKey,
	signOutProfileDevice,
	signOutProfileDevices,
	useListProfileDevices,
} from "@/api/gen/profiles/profiles";
import { useDialogs } from "@/components/dialogs";
import { Button } from "@/components/ui/button";
import { apiErrorMessage } from "@/lib/errors";
import { fmtDateTimeSecs } from "@/lib/format";
import { m } from "@/paraglide/messages";
import { deviceName } from "@/sections/Devices/pairedRows";

/** How a signed-in device gets back on, in plain words. */
export const KIND_WORD: Record<SignedInKind, () => string> = {
	for_now: () => m.profiles_device_for_now(),
	stay: () => m.profiles_device_stay(),
	added: () => m.profiles_device_added(),
	key: () => m.profiles_device_key(),
};

/**
 * Signs `fingerprint`, or every device for `null`, out of profile `id`. The rows leave the list at
 * once and come back when the host refuses; the refusal is rethrown for the caller to show.
 */
export async function signOut(
	qc: QueryClient,
	id: string,
	fingerprint: string | null,
): Promise<void> {
	const key = getListProfileDevicesQueryKey(id);
	await qc.cancelQueries({ queryKey: key });
	const before = qc.getQueryData<SignedInDevice[]>(key);
	qc.setQueryData<SignedInDevice[]>(key, (list) =>
		fingerprint ? list?.filter((d) => d.fingerprint !== fingerprint) : [],
	);
	try {
		await (fingerprint
			? signOutProfileDevice(id, fingerprint)
			: signOutProfileDevices(id));
	} catch (e) {
		qc.setQueryData(key, before);
		throw e;
	} finally {
		void qc.invalidateQueries({ queryKey: key });
	}
}

/** The card's **Signed-in devices** for `profile`, with **Sign out** behind a confirmation. */
export const SignedInDevices: FC<{ profile: ProfileAdmin }> = ({
	profile: p,
}) => {
	const qc = useQueryClient();
	const { confirm } = useDialogs();
	const devices = useListProfileDevices(p.id, {
		query: { refetchInterval: 15_000 },
	});
	const account = p.seat?.account ?? p.display_name;
	const ask = async (
		fingerprint: string | null,
		dialog: { title: string; description: string; confirmLabel: string },
	) => {
		if (!(await confirm({ ...dialog, destructive: true }))) return;
		try {
			await signOut(qc, p.id, fingerprint);
		} catch (e) {
			toast.error(apiErrorMessage(e) ?? m.profiles_devices_failed());
		}
	};
	return (
		<DevicesList
			account={account}
			devices={devices.data}
			error={
				devices.error
					? (apiErrorMessage(devices.error) ?? m.common_error())
					: null
			}
			onSignOut={(d) =>
				ask(d.fingerprint, {
					title: m.profiles_device_sign_out_title({ device: deviceName(d) }),
					description: m.profiles_device_sign_out_body({ account }),
					confirmLabel: m.profiles_device_sign_out(),
				})
			}
			onSignOutAll={() =>
				ask(null, {
					title: m.profiles_devices_sign_out_all_title(),
					description: m.profiles_devices_sign_out_all_body({ account }),
					confirmLabel: m.profiles_devices_sign_out_all(),
				})
			}
		/>
	);
};

/** The list itself: who may open the desktop, then each device, or one line when there is none. */
export const DevicesList: FC<{
	account: string;
	/** `undefined` while the first answer is on its way. */
	devices: SignedInDevice[] | undefined;
	/** The host's sentence when the list couldn't be read. */
	error: string | null;
	onSignOut: (d: SignedInDevice) => void;
	onSignOutAll: () => void;
}> = ({ account, devices, error, onSignOut, onSignOutAll }) => (
	<div className="space-y-2 sm:pl-14">
		<div className="flex items-center justify-between gap-4">
			<h3 className="text-sm font-medium">{m.profiles_devices_title()}</h3>
			{!error && devices && devices.length > 0 && (
				<Button variant="outline" size="sm" onClick={onSignOutAll}>
					{m.profiles_devices_sign_out_all()}
				</Button>
			)}
		</div>
		<p className="text-sm text-muted-foreground">
			{m.profiles_devices_trust({ account })}
		</p>
		{error ? (
			<p className="text-sm text-destructive">{error}</p>
		) : devices?.length === 0 ? (
			<p className="text-sm text-muted-foreground">
				{m.profiles_devices_none()}
			</p>
		) : (
			<ul className="divide-y">
				{devices?.map((d) => (
					<li
						key={d.fingerprint}
						className="flex items-center justify-between gap-4 py-2"
					>
						<div className="min-w-0">
							<p className="truncate text-sm">{deviceName(d)}</p>
							<p className="text-xs text-muted-foreground">
								{[
									KIND_WORD[d.kind]?.(),
									m.profiles_device_last_used({
										when: fmtDateTimeSecs(d.last_unix),
									}),
								]
									.filter(Boolean)
									.join(" · ")}
							</p>
						</div>
						<Button variant="ghost" size="sm" onClick={() => onSignOut(d)}>
							{m.profiles_device_sign_out()}
						</Button>
					</li>
				))}
			</ul>
		)}
	</div>
);
