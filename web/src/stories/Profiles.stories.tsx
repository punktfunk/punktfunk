import type { Meta, StoryObj } from "@storybook/react-vite";
import {
	AddProfileDialog,
	ProfilesView,
	RemoveProfileDialog,
} from "@/sections/Profiles/view";
import { profilesEvery, profilesOne } from "./lib/fixtures";

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
