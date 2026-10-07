import type { Meta, StoryObj } from "@storybook/react-vite";
import { KeyRound } from "lucide-react";
import { Button } from "@/components/ui/button";
import { MoonlightPairing } from "@/sections/Devices/MoonlightPairingCard";
import {
	type PairedRow,
	PairedRowView,
} from "@/sections/Devices/PairedDevices";
import { type NativePairing, PairSheet } from "@/sections/Devices/PairSheet";
import { PendingRow, WaitingRow } from "@/sections/Devices/PendingDevices";
import { DevicesView } from "@/sections/Devices/view";
import {
	accessNowUnix,
	nativeClients,
	nativePairArmed,
	pairedClients,
	pairingIdle,
	pendingDevices,
	pendingWithProfile,
} from "./lib/fixtures";

const noop = () => {};
const idle = { isLoading: false, error: null, refetch: noop };

/** The fixture clients as rows, access fields carried along (what the container maps). */
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

const pairedRow = (r: PairedRow, i: number) => (
	<PairedRowView
		key={`${r.protocol}:${r.fingerprint}`}
		row={r}
		nowUnix={accessNowUnix}
		display={
			r.protocol === "native"
				? i === 0
					? "Takes over · Kept 10 s"
					: "Follows host"
				: undefined
		}
		streaming={i === 0}
		busy={false}
		onEditAccess={r.protocol === "native" ? noop : undefined}
		onDisplaySettings={r.protocol === "native" ? noop : undefined}
		onRename={r.protocol === "moonlight" ? noop : undefined}
		onUnpair={noop}
	/>
);

const pairing = (pin: string | null): NativePairing => ({
	status: {
		data: pin ? nativePairArmed : { ...nativePairArmed, armed: false },
		...idle,
	} as NativePairing["status"],
	pin,
	onArm: noop,
	onDisarm: noop,
	isArming: false,
	failure: null,
	isDisarming: false,
});

const meta = {
	title: "Pages/Devices",
	component: DevicesView,
	args: {
		actions: <Button>Pair a device</Button>,
		waiting: [
			<WaitingRow
				key="armed"
				lead={<KeyRound className="size-4" />}
				title="PIN 4827 · expires in 1:38"
				actions={
					<Button size="sm" variant="outline">
						Cancel
					</Button>
				}
			/>,
			...[...pendingDevices, pendingWithProfile].map((p) => (
				<PendingRow
					key={p.id}
					device={p}
					onApprove={noop}
					onArmFor={noop}
					onDeny={noop}
					busy={false}
				/>
			)),
		],
		paired: [...nativeRows, ...moonlightRows].map(pairedRow),
		pairedState: idle,
	},
} satisfies Meta<typeof DevicesView>;

export default meta;
type Story = StoryObj<typeof meta>;

/** Knocks first, an armed PIN, then every paired device on both planes. */
export const Armed: Story = {};

/** Nothing waiting, nothing paired: one line. */
export const Empty: Story = { args: { waiting: [], paired: [] } };

const moonlight = (
	<MoonlightPairing
		pairing={{ data: pairingIdle, ...idle }}
		pin=""
		onPinChange={noop}
		label=""
		onLabelChange={noop}
		password=""
		onPasswordChange={noop}
		failure={null}
		target=""
		onTargetChange={noop}
		onSubmit={noop}
		isSubmitting={false}
		isSuccess={false}
		isError={false}
	/>
);

/** The Pair sheet: access for the next device, the console password, and the Moonlight step. */
export const PairSheetArm: Story = {
	render: () => (
		<PairSheet
			open
			onOpenChange={noop}
			pairing={pairing(null)}
			boundTo={null}
			onClearBound={noop}
			moonlight={moonlight}
			step="app"
			onStep={noop}
		/>
	),
};

/** The PIN the device enters, counting down. */
export const PairSheetPin: Story = {
	render: () => (
		<PairSheet
			open
			onOpenChange={noop}
			pairing={pairing(nativePairArmed.pin ?? "4827")}
			boundTo={null}
			onClearBound={noop}
			step="app"
			onStep={noop}
		/>
	),
};

/** GameStream on: the sheet's second step takes a Moonlight client's PIN. */
export const PairSheetMoonlight: Story = {
	render: () => (
		<PairSheet
			open
			onOpenChange={noop}
			pairing={pairing(null)}
			boundTo={null}
			onClearBound={noop}
			moonlight={moonlight}
			step="moonlight"
			onStep={noop}
		/>
	),
};

/** Every Access state at once: full/permanent, a live countdown, a custom mask, an Expired row
 * (kept listed), a host OLDER than the access fields ("—"), and Moonlight's "Full (ungoverned)". */
export const AccessColumn: Story = {
	args: {
		waiting: [],
		paired: [
			...nativeRows,
			{
				protocol: "native" as const,
				fingerprint:
					"c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00",
				name: "media-remote",
				accessLevel: "custom",
				grants: 0x09, // controller + clipboard
				expiresUnix: accessNowUnix + 26 * 3600,
			},
			{
				protocol: "native" as const,
				fingerprint:
					"0011223344556677889900aabbccddeeff102030405060708090a0b0c0d0e0f0",
				name: "leons-deck",
				accessLevel: "controller",
				grants: 0x01,
				expiresUnix: accessNowUnix - 2 * 3600,
			},
			{
				protocol: "native" as const,
				fingerprint:
					"9f8e7d6c5b4a39281706f5e4d3c2b1a0998877665544332211ffeeddccbbaa00",
				name: "old-host-row",
			},
			...moonlightRows,
		].map(pairedRow),
	},
};
