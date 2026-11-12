import type { Meta, StoryObj } from "@storybook/react-vite";
import type { SessionSettingsState } from "@/api/gen/model/sessionSettingsState";
import { GameExitFixed, GameExitView } from "@/sections/Profiles/session";
import {
	AddProfileDialog,
	DoctorDialog,
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
	seatsDoctor,
} from "./lib/fixtures";

const noop = () => {};
const done = async () => {};

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

/** Linux without the door: the owner and two desktop sharers, one of whom has a Steam kept on
 * this host. */
export const EveryState: Story = {};

/** One person on the box: the owner's card and **Add profile**, nothing else. */
export const OneProfile: Story = {
	args: { profiles: { data: profilesOne, isLoading: false, error: null } },
};

/** **Add profile** on Linux without the door: sharing the desktop, own desktop waiting for it. */
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

/** **Remove** a profile this host keeps a Steam for: the console password, and `erase` spelled
 * out. */
export const RemoveKeepsSteam: Story = {
	render: () => (
		<RemoveProfileDialog
			profile={profilesEvery[2] ?? null}
			onCancel={noop}
			onRemove={done}
		/>
	),
};

const seats = {
	onSeats: noop,
	onStart: noop,
	onStop: noop,
	onEnd: noop,
	onPassword: noop,
	doctor: { message: null, notes: 0, onOpen: noop },
};

/** A Windows Server host: a seat ready, in use, starting, stopped and unavailable. */
export const WindowsEveryState: Story = {
	args: {
		profiles: { data: profilesWindows, isLoading: false, error: null },
		seats,
	},
};

/** The doctor found a problem: its first error, and **Doctor** to see every check. */
export const WindowsDoctorError: Story = {
	args: {
		profiles: { data: profilesWindows, isLoading: false, error: null },
		seats: {
			...seats,
			doctor: {
				message:
					"The seat display driver isn't installed. Seats put it back when they next start; if this stays, turn seats off and on under Profiles → Seats.",
				notes: 1,
				onOpen: noop,
			},
		},
	},
};

/** **Doctor**: the PC's checks, then one group per seat, the worst first. */
export const Doctor: Story = {
	render: () => (
		<DoctorDialog
			open
			doctor={seatsDoctor}
			error={null}
			checking={false}
			docs="windows-seats#troubleshooting"
			onRecheck={noop}
			onClose={noop}
		/>
	),
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

/** A fresh Windows Server: the failing lines first, and **Set up Remote Desktop** to clear them. */
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

/** **Set up Remote Desktop** under way: installing the role takes minutes. */
export const SeatsPreparing: Story = {
	render: () => (
		<SeatsDialog
			open
			seating={seatingRefused}
			isPending
			preparing
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

/** A box that can't turn the door on yet: the host's sentence says what to do, the switch waits. */
export const LinuxDoorUnavailable: Story = {
	args: {
		door: {
			on: false,
			changing: false,
			unavailable:
				"This install is missing the punktfunk-seats package. Install it: sudo pacman -S punktfunk-seats",
			onChange: noop,
		},
	},
};

/** The switch under way: the host and the console restart, then the page answers from the other. */
export const LinuxDoorSwitching: Story = {
	args: { door: { on: false, changing: true, onChange: noop } },
};

/** Turning the door on asks for the console password. */
export const DoorTurnOn: Story = {
	render: () => <DoorDialog open turningOn onConfirm={done} onCancel={noop} />,
};

/** Turning it off hands the box back to the owner's session. */
export const DoorTurnOff: Story = {
	render: () => (
		<DoorDialog open turningOn={false} onConfirm={done} onCancel={noop} />
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

/** A profile's own **When the game exits**, as the host answers it. */
const exit = (
	configured: boolean,
	end: boolean,
	enforced = ["session_on_game_exit"],
): SessionSettingsState => ({
	configured,
	enforced,
	settings: { session_on_game_exit: end },
});

/**
 * **When the game exits** on each card whose seat has a host of its own: Kid follows the host,
 * Leon set his own (the host can't drop it yet), Mia's is on its way, Oma's couldn't be read.
 */
export const SessionPreference: Story = {
	args: {
		profiles: { data: profilesWindows, isLoading: false, error: null },
		seats,
		session: (p) => {
			const at = (state: SessionSettingsState | undefined, more = {}) => (
				<GameExitView
					state={state}
					loading={false}
					error={null}
					busy={false}
					onPick={noop}
					{...more}
				/>
			);
			switch (p.display_name) {
				case "Kid":
					return at(exit(false, true));
				case "Leon":
					return at(exit(true, false));
				case "Mia":
					return at(undefined, { loading: true });
				case "Oma":
					return at(undefined, {
						error: "That host didn't answer. Try again in a moment.",
					});
				case "Alice":
					return at(exit(false, false));
				default:
					return null;
			}
		},
	},
};

/** A door: Leon's own desktop has the choice; Oma plays on her own Linux account, where a
 * game's exit always goes back to her desktop. */
export const SessionPreferenceDoor: Story = {
	args: {
		profiles: {
			data: profilesDoor.map((p) =>
				p.display_name === "Oma" && p.seat
					? { ...p, seat: { ...p.seat, account: "oma" } }
					: p,
			),
			isLoading: false,
			error: null,
		},
		seats,
		door: { on: true, changing: false, onChange: noop },
		session: (p) =>
			p.display_name === "Oma" ? (
				<GameExitFixed name={p.display_name} />
			) : p.display_name === "Leon" ? (
				<GameExitView
					state={exit(false, true)}
					loading={false}
					error={null}
					busy={false}
					onPick={noop}
				/>
			) : null,
	},
};

/** **Remove** a Windows seat profile: no choice to keep its account, and the password. */
export const RemoveWindowsSeat: Story = {
	render: () => (
		<RemoveProfileDialog
			profile={profilesWindows[2] ?? null}
			windows
			onCancel={noop}
			onRemove={done}
		/>
	),
};
