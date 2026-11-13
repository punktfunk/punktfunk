import type { Meta, StoryObj } from "@storybook/react-vite";
import type { ComponentProps } from "react";
import { expect, userEvent, within } from "storybook/test";
import type { LockSummary } from "@/api/gen/model/lockSummary";
import type { ProfileAdmin } from "@/api/gen/model/profileAdmin";
import type { SignedInDevice } from "@/api/gen/model/signedInDevice";
import { useDialogs } from "@/components/dialogs";
import { m } from "@/paraglide/messages";
import { DevicesList } from "@/sections/Profiles/devices";
import {
	LockFlowDialog,
	LockManageDialog,
	type LockRow,
	signOutAsk,
} from "@/sections/Profiles/lock";
import { ProfilesView } from "@/sections/Profiles/view";
import {
	accessNowUnix,
	profilesDoor,
	profilesEvery,
	profilesWindows,
} from "./lib/fixtures";

const noop = () => {};
const done = async () => {};
const fp = (c: string) => c.repeat(64);

const TV: LockRow = {
	fingerprint: fp("a"),
	name: "Living room TV",
	moonlight: false,
};
const DECK: LockRow = {
	fingerprint: fp("b"),
	name: "Kid's Deck",
	moonlight: false,
};
const PHONE: LockRow = {
	fingerprint: fp("c"),
	name: "Enrico's iPhone",
	moonlight: false,
};
const SHIELD: LockRow = {
	fingerprint: fp("d"),
	name: "Shield in the bedroom",
	moonlight: true,
};
const PAIRED = [TV, DECK, PHONE, SHIELD];

const signedIn = (
	row: LockRow,
	kind: SignedInDevice["kind"],
	ago: number,
): SignedInDevice => ({
	fingerprint: row.fingerprint,
	name: row.name,
	kind,
	added_unix: accessNowUnix - 30 * 86_400,
	last_unix: accessNowUnix - ago,
	moonlight: row.moonlight,
});

const OPEN: LockSummary = { locked: false, pin: false, failed_pins: 0 };
const OWNER_ID = "4f1c3a9b0e27";

/**
 * `list` as the host lists it: each profile's own lock by name, a sharer of the owner's desktop
 * with the owner's, and a person's own account (`own`) with none.
 */
const listed = (
	list: ProfileAdmin[],
	by: Record<string, LockSummary> = {},
	own: string[] = [],
): ProfileAdmin[] => {
	const owner = by[list.find((p) => p.owner)?.display_name ?? ""] ?? OPEN;
	return list.map((p) => {
		if (own.includes(p.display_name)) return p;
		const sharer = !p.owner && (!p.seat || p.seat.kind === "shared");
		const lock = sharer
			? { ...owner, shared_with: OWNER_ID }
			: (by[p.display_name] ?? OPEN);
		return { ...p, lock };
	});
};
const loaded = (data: ProfileAdmin[]) => ({
	data,
	isLoading: false,
	error: null,
});
const locked = (failed_pins = 0): LockSummary => ({
	locked: true,
	pin: failed_pins > 0,
	failed_pins,
});

const meta = {
	title: "Pages/Profile lock",
	component: ProfilesView,
	parameters: { layout: "padded" },
	args: {
		profiles: loaded(listed(profilesEvery)),
		onAdd: noop,
		onRename: noop,
		onPicture: noop,
		onRemovePicture: noop,
		onRemove: noop,
		onLock: noop,
		busyId: null,
	},
} satisfies Meta<typeof ProfilesView>;

export default meta;
type Story = StoryObj<typeof meta>;

/** Nothing locked on a Linux box: one lock icon per card and nothing else. Anna and Kid share
 * Enrico's desktop, so theirs locks Enrico's. */
export const CardsNothingLocked: Story = {};

/** The list says Enrico's desktop is locked, with three wrong PINs: Anna and Kid open with his
 * lock. */
export const CardsOwnerLocked: Story = {
	args: { profiles: loaded(listed(profilesEvery, { Enrico: locked(3) })) },
};

/** A door with Oma on her own account: her signed-in devices stay as they are, Leon's desktop is
 * locked, the rest are open. */
export const CardsDoor: Story = {
	args: {
		profiles: loaded(
			listed(
				profilesDoor.map((p) =>
					p.display_name === "Oma" && p.seat
						? { ...p, seat: { ...p.seat, account: "oma" } }
						: p,
				),
				{ Leon: locked() },
				["Oma"],
			),
		),
		door: { on: true, changing: false, onChange: noop },
		devices: () => (
			<DevicesList
				account="oma"
				devices={[signedIn(PHONE, "key", 3600), signedIn(TV, "stay", 86_400)]}
				error={null}
				onSignOut={noop}
				onSignOutAll={noop}
			/>
		),
	},
};

/** A Windows Server host: Kid's seat is locked, every other own desktop has the lock icon. */
export const CardsWindows: Story = {
	args: { profiles: loaded(listed(profilesWindows, { Kid: locked(1) })) },
};

/** The lock dialog for `name`, as the host answers it. */
const flow = (more: Partial<ComponentProps<typeof LockFlowDialog>> = {}) => (
	<LockFlowDialog
		name="Kid"
		owner={false}
		sharers={false}
		rows={PAIRED}
		kept={[DECK.fingerprint]}
		playing={[{ fingerprint: DECK.fingerprint, name: DECK.name }]}
		hasPin={false}
		error={null}
		onRetry={noop}
		onRemovePin={noop}
		onLock={done}
		onClose={noop}
		{...more}
	/>
);

const dialog = () => within(document.body);

/** **Lock**: Kid's Deck plays as Kid right now, so it is chosen already. */
export const LockFlow: Story = { render: () => flow() };

/** Unchoosing the Deck that plays now names it before the lock, and the button turns red. */
export const LockFlowCutsOff: Story = {
	render: () => flow(),
	play: async () => {
		await userEvent.click(
			await dialog().findByRole("checkbox", { name: /Kid's Deck/ }),
		);
		await expect(
			await dialog().findByText(m.profiles_lock_cut_off({ device: DECK.name })),
		).toBeVisible();
	},
};

/** Locking Enrico's own desktop: Moonlight devices need a choice, and his sharers come along. */
export const LockFlowOwner: Story = {
	render: () =>
		flow({
			name: "Enrico",
			owner: true,
			sharers: true,
			kept: [PHONE.fingerprint, SHIELD.fingerprint],
			playing: [
				{ fingerprint: PHONE.fingerprint, name: PHONE.name },
				{ fingerprint: SHIELD.fingerprint, name: SHIELD.name },
			],
		}),
};

/** A PIN typed: the console password joins the dialog, since a PIN lets more devices in. */
export const LockFlowWithPin: Story = {
	render: () => flow(),
	play: async () => {
		await userEvent.type(
			await dialog().findByLabelText(m.profiles_pin_optional()),
			"2468",
		);
		await dialog().findByLabelText(m.store_spec_password());
	},
};

/** A PIN that is too short, once left: the host's own words say why, and Lock waits. */
export const LockFlowPinRefused: Story = {
	render: () => flow(),
	play: async () => {
		await userEvent.type(
			await dialog().findByLabelText(m.profiles_pin_optional()),
			"12",
		);
		await userEvent.tab();
		await dialog().findByText(m.apierr_profile_pin_invalid());
	},
};

/** Nobody plays and nothing is chosen: the dialog says no device will open Kid. */
export const LockFlowNobody: Story = {
	render: () => flow({ kept: [], playing: [] }),
};

/** A session that matches no paired device can't be kept: it is named, and Lock turns red. */
export const LockFlowUnmatched: Story = {
	render: () =>
		flow({
			playing: [
				{ fingerprint: DECK.fingerprint, name: DECK.name },
				{ name: "Shield TV" },
			],
		}),
};

/** Locked before: the devices it let in then are chosen again, and its PIN stays unless removed
 * here. The lock holds exactly what is chosen. */
export const LockFlowRelock: Story = {
	render: () =>
		flow({
			kept: [DECK.fingerprint, TV.fingerprint],
			playing: [],
			hasPin: true,
		}),
};

/** The paired devices are on their way. */
export const LockFlowLoading: Story = {
	render: () => flow({ rows: undefined }),
};

/** No device is paired with this host yet. */
export const LockFlowNoDevices: Story = {
	render: () => flow({ rows: [], kept: [], playing: [] }),
};

/** A list of devices didn't load: its sentence and **Retry**, and Lock waits for it. */
export const LockFlowListFailed: Story = {
	render: () =>
		flow({
			rows: [DECK],
			error: "The host didn't answer. Try again in a moment.",
		}),
};

const LAPTOP: LockRow = {
	fingerprint: fp("e"),
	name: "Office laptop",
	moonlight: false,
};
const BEDROOM: LockRow = {
	fingerprint: fp("f"),
	name: "Bedroom TV",
	moonlight: true,
};

/** One device of each kind: two the operator added, a Moonlight one among them. */
const DEVICES = [
	signedIn(DECK, "added", 600),
	signedIn(SHIELD, "added", 2 * 86_400),
	signedIn(LAPTOP, "stay", 5 * 86_400),
	signedIn(PHONE, "key", 3 * 86_400),
	signedIn(TV, "for_now", 120),
];

/** The dialog that manages Kid's lock. */
const manage = (
	more: Partial<ComponentProps<typeof LockManageDialog>> = {},
) => (
	<LockManageDialog
		name="Kid"
		lock={{ locked: true, pin: true, failed_pins: 3, playing: [] }}
		lockError={null}
		devices={DEVICES}
		devicesError={null}
		onRetry={noop}
		candidates={[BEDROOM]}
		onSignOut={noop}
		onSignOutAll={noop}
		onAdd={done}
		onSetPin={done}
		onRemovePin={noop}
		onUnlock={done}
		onClose={noop}
		{...more}
	/>
);

/** Kid's dialog whose **Sign out** asks as the page does. */
const AsksSignOut = () => {
	const { confirm } = useDialogs();
	return manage({
		onSignOut: (d) => void confirm(signOutAsk("Kid", true, d)),
	});
};

/** Kid is locked: each device with how it got in, Moonlight ones tagged, a PIN, and three wrong
 * PINs. */
export const Manage: Story = { render: () => manage() };

/** No device and no PIN: nobody can play as Kid, and the dialog says what to do. */
export const ManageEmpty: Story = {
	render: () =>
		manage({
			devices: [],
			lock: { locked: true, pin: false, failed_pins: 0, playing: [] },
		}),
};

/** No device yet, but a PIN opens it. */
export const ManageEmptyWithPin: Story = {
	render: () =>
		manage({
			devices: [],
			lock: { locked: true, pin: true, failed_pins: 0, playing: [] },
		}),
};

/** No device on the list and none paired to add. */
export const ManageNothingPaired: Story = {
	render: () =>
		manage({
			devices: [],
			candidates: [],
			lock: { locked: true, pin: false, failed_pins: 0, playing: [] },
		}),
};

/** The lock and its devices are on their way. */
export const ManageLoading: Story = {
	render: () => manage({ lock: undefined, devices: undefined }),
};

/** The host can't read its lock file: its sentence and **Retry**. */
export const ManageFailed: Story = {
	render: () =>
		manage({
			lock: undefined,
			devices: undefined,
			lockError:
				"This host couldn't read its profile locks, so every profile stays locked. Fix or remove profile-locks.json, then try again.",
		}),
};

/** **Sign out**: the device and what happens to it. */
export const ManageSignOut: Story = {
	render: () => <AsksSignOut />,
	play: async () => {
		const [first] = await dialog().findAllByRole("button", {
			name: m.profiles_device_sign_out(),
		});
		if (first) await userEvent.click(first);
		await dialog().findByText(
			m.profiles_device_sign_out_title({ device: DECK.name }),
		);
	},
};

/** **Add a device**: one paired device that isn't on the list, and the console password. */
export const ManageAddDevice: Story = {
	render: () => manage({ step: "add" }),
};

/** **Set PIN** on a profile without one. */
export const ManageSetPin: Story = {
	render: () =>
		manage({
			step: "pin",
			lock: { locked: true, pin: false, failed_pins: 0, playing: [] },
		}),
};

/** **Change PIN** with a value the host would refuse: its words, and the button waits. */
export const ManageChangePinRefused: Story = {
	render: () => manage({ step: "pin" }),
	play: async () => {
		await userEvent.type(
			await dialog().findByLabelText(m.profiles_pin_label()),
			"12",
		);
		await userEvent.tab();
		await dialog().findByText(m.apierr_profile_pin_invalid());
	},
};

/** **Unlock** asks for the console password. */
export const ManageUnlock: Story = {
	render: () => manage({ step: "unlock" }),
};
