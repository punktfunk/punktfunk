import type { Meta, StoryObj } from "@storybook/react-vite";
import { ApproveDialog } from "@/sections/Devices/ApproveDialog";
import { EditAccessSheet } from "@/sections/Devices/EditAccessSheet";
import {
	PairedDevices,
	type PairedRow,
} from "@/sections/Devices/PairedDevices";
import {
	accessNowUnix,
	nativeClients,
	pairedClients,
	pendingDevices,
	pendingGuestReknock,
	pendingWithProfile,
} from "./lib/fixtures";

const noop = () => {};

/** The fixture clients as table rows, access fields carried along (what the container maps). */
const nativeRows: PairedRow[] = nativeClients.map((c) => ({
	protocol: "native" as const,
	fingerprint: c.fingerprint,
	name: c.name,
	accessLevel: c.access_level,
	grants: c.grants,
	expiresUnix: c.expires_unix,
}));

const moonlightRows: PairedRow[] = pairedClients.map((c) => ({
	protocol: "moonlight" as const,
	fingerprint: c.fingerprint,
	name: c.label ?? c.subject ?? "",
	label: c.label,
}));

// Per-client access states, separate from Pages/Pairing: these stories render single components
// (the two dialogs + the column matrix), not the page layout — an untyped meta, because
// PairingView's required slots would otherwise demand `args` every render-only story lacks.
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

/** Every Access-column state at once: full/permanent, a live countdown, a custom mask, an
 * Expired row (kept listed — D3), a row from a host OLDER than the access fields ("—", nothing
 * crashes), and the honest Moonlight "Full (ungoverned)" chip with no editor. */
export const AccessColumn: Story = {
	render: () => (
		<PairedDevices
			rows={[
				...nativeRows,
				{
					protocol: "native",
					fingerprint:
						"c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00",
					name: "media-remote",
					accessLevel: "custom",
					grants: 0x09, // controller + clipboard
					expiresUnix: accessNowUnix + 26 * 3600,
				},
				{
					protocol: "native",
					fingerprint:
						"0011223344556677889900aabbccddeeff102030405060708090a0b0c0d0e0f0",
					name: "leons-deck",
					accessLevel: "controller",
					grants: 0x01,
					expiresUnix: accessNowUnix - 2 * 3600,
				},
				{
					// A host from before per-client access reports none of the fields — the
					// column renders "—" and offers no editor.
					protocol: "native",
					fingerprint:
						"9f8e7d6c5b4a39281706f5e4d3c2b1a0998877665544332211ffeeddccbbaa00",
					name: "old-host-row",
				},
				...moonlightRows,
			]}
			isLoading={false}
			error={null}
			refetch={noop}
			nowUnix={accessNowUnix}
			onEditAccess={noop}
			onDisplaySettings={noop}
			perDevice
			onRename={noop}
			onUnpair={noop}
			onUnpairAll={noop}
			pendingFingerprint={null}
			isUnpairingAll={false}
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
