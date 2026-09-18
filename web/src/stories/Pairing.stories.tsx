import type { Meta, StoryObj } from "@storybook/react-vite";
import { MoonlightPairing } from "@/sections/Pairing/MoonlightPairingCard";
import { NativePairingCard } from "@/sections/Pairing/NativePairingCard";
import {
	PairedDevices,
	type PairedRow,
} from "@/sections/Pairing/PairedDevices";
import { PendingDevices } from "@/sections/Pairing/PendingDevices";
import { PairingView } from "@/sections/Pairing/view";
import {
	accessNowUnix,
	nativeClients,
	nativePairArmed,
	pairedClients,
	pairingIdle,
	pendingDevices,
} from "./lib/fixtures";

const noop = () => {};
const idle = { isLoading: false, error: null, refetch: noop };

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

// Renders the REAL page layout (PairingView) — the same component index.tsx uses. The live page
// fills its slots with the self-contained containers; here we fill them with the pure cards + mock
// state, so there's no duplicated composition to drift.
const meta = {
	title: "Pages/Pairing",
	component: PairingView,
	parameters: { layout: "padded" },
} satisfies Meta<typeof PairingView>;

export default meta;
type Story = StoryObj<typeof meta>;

// The marketing state: one device knocking for delegated approval, a PIN armed for a phone, the
// consolidated paired-devices list (native + Moonlight), idle Moonlight pairing.
export const Armed: Story = {
	args: {
		pending: (
			<PendingDevices
				pending={{ data: pendingDevices, ...idle }}
				onApprove={noop}
				onArmFor={noop}
				onDeny={noop}
				pendingId={null}
			/>
		),
		native: (
			<NativePairingCard
				status={{ data: nativePairArmed, ...idle }}
				// The armed PIN reaches the card from the arm RESPONSE, never from the polled status
				// (the BFF strips it) — so the story hands it in the same way.
				pin={nativePairArmed.pin ?? null}
				onArm={noop}
				onDisarm={noop}
				isArming={false}
				wrongPassword={false}
				isDisarming={false}
			/>
		),
		moonlight: (
			<MoonlightPairing
				pairing={{ data: pairingIdle, ...idle }}
				pin=""
				onPinChange={noop}
				label=""
				onLabelChange={noop}
				password=""
				onPasswordChange={noop}
				wrongPassword={false}
				target=""
				onTargetChange={noop}
				onSubmit={noop}
				isSubmitting={false}
				isSuccess={false}
				isError={false}
			/>
		),
		paired: (
			<PairedDevices
				rows={[...nativeRows, ...moonlightRows]}
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
	},
};
