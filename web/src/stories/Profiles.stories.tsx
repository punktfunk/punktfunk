import type { Meta, StoryObj } from "@storybook/react-vite";
import {
	AddProfileDialog,
	DoorDialog,
	ProfilesView,
	RemoveProfileDialog,
	SeatsDialog,
} from "@/sections/Profiles/view";
import {
	profilesDoor,
	profilesEvery,
	profilesOne,
	profilesWindows,
	seatingOn,
	seatingRefused,
} from "./lib/fixtures";

const noop = () => {};

const meta = {
	title: "Pages/Profiles",
	component: ProfilesView,
	parameters: { layout: "padded" },
	args: {
		profiles: { data: profilesEvery, isLoading: false, error: null },
		onAdd: noop,
		onRename: noop,
		onPicture: noop,
		onRemovePicture: noop,
		onRemove: noop,
		busyId: null,
	},
} satisfies Meta<typeof ProfilesView>;

export default meta;
type Story = StoryObj<typeof meta>;

/** Every card state on Linux: owner, desktop sharer, and a light seat ready, needing its Steam
 * sign-in, in use, starting, stopped and unavailable. */
export const EveryState: Story = {};

/** One person on the box: the owner's card and **Add profile**, nothing else. */
export const OneProfile: Story = {
	args: { profiles: { data: profilesOne, isLoading: false, error: null } },
};

/** **Add profile** on Linux: own Steam offered, own desktop waiting for the seats package. */
export const AddOnLinux: Story = {
	render: () => (
		<AddProfileDialog
			open
			ownerName="Enrico"
			linux
			onCancel={noop}
			onCreate={noop}
			isPending={false}
		/>
	),
};

/** **Add profile** while **Steam per seat** is untouched: the first Own Steam profile turns it on. */
export const AddTurnsOnSteamPerSeat: Story = {
	render: () => (
		<AddProfileDialog
			open
			ownerName="Enrico"
			linux
			seatHome="turns-on"
			onCancel={noop}
			onCreate={noop}
			isPending={false}
		/>
	),
};

/** **Steam per seat** set off by the operator: seat profiles say they need it. */
export const SteamPerSeatOff: Story = { args: { seatHome: "off" } };

/** **Add profile** on a host that is not Linux: sharing the desktop is the one choice today. */
export const AddElsewhere: Story = {
	render: () => (
		<AddProfileDialog
			open
			ownerName="Enrico"
			linux={false}
			onCancel={noop}
			onCreate={noop}
			isPending={false}
		/>
	),
};

/** **Remove** a seat profile: the console password, and `erase` spelled out. */
export const RemoveSeat: Story = {
	render: () => (
		<RemoveProfileDialog
			profile={profilesEvery[2] ?? null}
			onCancel={noop}
			onRemove={noop}
			isPending={false}
			failure={null}
		/>
	),
};

const seats = {
	onSeats: noop,
	onStart: noop,
	onStop: noop,
	onEnd: noop,
	doctor: { message: null, checking: false, onRun: noop },
};

/** A Windows Server host: a seat ready, in use, starting, stopped and unavailable. */
export const WindowsEveryState: Story = {
	args: {
		profiles: { data: profilesWindows, isLoading: false, error: null },
		seats,
	},
};

/** The doctor found a problem: its first error, and **Doctor** to run it again. */
export const WindowsDoctorError: Story = {
	args: {
		profiles: { data: profilesWindows, isLoading: false, error: null },
		seats: {
			...seats,
			doctor: {
				message: "Remote Desktop licensing has run out for this server.",
				checking: false,
				onRun: noop,
			},
		},
	},
};

/** **Seats** with seats on: the switch and what the checks found. */
export const SeatsOn: Story = {
	render: () => (
		<SeatsDialog
			open
			seating={seatingOn}
			isPending={false}
			onChange={noop}
			onClose={noop}
		/>
	),
};

/** A turn-on the checks refused: seats stay off and the failing line comes first. */
export const SeatsRefused: Story = {
	render: () => (
		<SeatsDialog
			open
			seating={seatingRefused}
			isPending={false}
			onChange={noop}
			onClose={noop}
		/>
	),
};

/** **Add profile** on Windows with seats on: **Own desktop** is offered. */
export const AddOnWindows: Story = {
	render: () => (
		<AddProfileDialog
			open
			ownerName="Enrico"
			linux={false}
			windows
			seatsOn
			onCancel={noop}
			onCreate={noop}
			isPending={false}
		/>
	),
};

/** **Add profile** on Windows with seats off: **Own desktop** waits for them. */
export const AddOnWindowsSeatsOff: Story = {
	render: () => (
		<AddProfileDialog
			open
			ownerName="Enrico"
			linux={false}
			windows
			onCancel={noop}
			onCreate={noop}
			isPending={false}
		/>
	),
};

/** A Linux door: the switch is on, the owner's seat waits for a device, and seats of their own run. */
export const LinuxDoorOn: Story = {
	args: {
		profiles: { data: profilesDoor, isLoading: false, error: null },
		seats,
		door: { on: true, changing: false, onChange: noop },
	},
};

/** A Linux box without the door: the switch is off and nothing else changes. */
export const LinuxDoorOff: Story = {
	args: { door: { on: false, changing: false, onChange: noop } },
};

/** The switch under way: the host and the console restart, then the page answers from the other. */
export const LinuxDoorSwitching: Story = {
	args: { door: { on: false, changing: true, onChange: noop } },
};

/** Turning the door on asks for the console password. */
export const DoorTurnOn: Story = {
	render: () => (
		<DoorDialog
			open
			turningOn
			isPending={false}
			failure={null}
			onConfirm={noop}
			onCancel={noop}
		/>
	),
};

/** Turning it off hands the box back to the owner's session. */
export const DoorTurnOff: Story = {
	render: () => (
		<DoorDialog
			open
			turningOn={false}
			isPending={false}
			failure="wrong"
			onConfirm={noop}
			onCancel={noop}
		/>
	),
};

/** **Add profile** with the door on: **Own desktop** is a user of its own. */
export const AddOnLinuxDoor: Story = {
	render: () => (
		<AddProfileDialog
			open
			ownerName="Enrico"
			linux
			door
			onCancel={noop}
			onCreate={noop}
			isPending={false}
		/>
	),
};

/** **Remove** a Windows seat profile: no choice to keep its account, and the password. */
export const RemoveWindowsSeat: Story = {
	render: () => (
		<RemoveProfileDialog
			profile={profilesWindows[2] ?? null}
			windows
			onCancel={noop}
			onRemove={noop}
			isPending={false}
			failure={null}
		/>
	),
};
