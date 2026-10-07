import type { Meta, StoryObj } from "@storybook/react-vite";
import { ApproveDialog } from "@/sections/Devices/ApproveDialog";
import { EditAccessSheet } from "@/sections/Devices/EditAccessSheet";
import {
	accessNowUnix,
	pendingDevices,
	pendingGuestReknock,
	pendingWithProfile,
} from "./lib/fixtures";

const noop = () => {};

// Per-client access dialogs, separate from Pages/Devices: single components, not the page layout.
const meta: Meta = {
	title: "Pages/PairingAccess",
	parameters: { layout: "padded" },
};

export default meta;
type Story = StoryObj;

/** The approve dialog for an UNKNOWN device: defaults Full control · Never (D1), with the
 * one-click "Approve as guest" fast path (Controller only · 4 h). */
export const ApproveDevice: Story = {
	render: () => (
		<ApproveDialog
			device={pendingDevices[0] ?? null}
			onCancel={noop}
			onApprove={noop}
			isPending={false}
			failure={null}
		/>
	),
};

/** A knock that names a profile: the dialog says who the device asks to play as. */
export const ApproveForProfile: Story = {
	render: () => (
		<ApproveDialog
			device={pendingWithProfile}
			onCancel={noop}
			onApprove={noop}
			isPending={false}
			failure={null}
		/>
	),
};

/** The expired-guest re-knock: the fingerprint is already stored (Controller only · 4 h, now
 * past), so the dialog pre-fills "re-grant what they had" and says the device is known. */
export const ApproveReknock: Story = {
	render: () => (
		<ApproveDialog
			device={pendingGuestReknock}
			onCancel={noop}
			onApprove={noop}
			isPending={false}
			failure={null}
		/>
	),
};

/** The row edit sheet: preset + Advanced toggles, keep/extend/never expiry, expire now, remove. */
export const EditAccess: Story = {
	render: () => (
		<EditAccessSheet
			target={{
				fingerprint:
					"ff00eeddccbbaa998877665544332211009f8e7d6c5b4a39281706f5e4d3c2b1",
				name: "living-room-tv",
				grants: 0x01,
				expiresUnix: accessNowUnix + 7080,
			}}
			nowUnix={accessNowUnix}
			onCancel={noop}
			onSave={noop}
			onExpireNow={noop}
			onRemove={noop}
			isPending={false}
		/>
	),
};
