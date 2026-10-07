import { type FC, useCallback, useState } from "react";
import { useGetHostInfo } from "@/api/gen/host/host";
import type { PendingDevice } from "@/api/gen/model/pendingDevice";
import { useLocale } from "@/lib/i18n";
import { MoonlightPairingSection } from "./MoonlightPairingCard";
import { type BoundDevice, NativePairingSection } from "./NativePairingCard";
import { PairedDevicesSection } from "./PairedDevices";
import { PendingDevicesSection } from "./PendingDevices";
import { PairingView } from "./view";

// Pairing composes four independent, self-contained sub-cards. Each subsection owns its own
// queries + mutations (in its own file, next to its presentational card). The arrangement lives in
// PairingView so the live page (these containers) and the Storybook story (pure cards + mock state)
// fill the same slots — the layout is defined once and can't drift.
export const SectionDevices: FC = () => {
	useLocale();
	// A knock from the internet cannot be approved by name, so its row hands the fingerprint to
	// the arm card and the operator reads the PIN out. Held here because the two cards are
	// siblings, and the armed PIN only ever exists in the arm response.
	const [armFor, setArmFor] = useState<BoundDevice | null>(null);
	const bindTo = (device: PendingDevice) =>
		setArmFor({ fingerprint: device.fingerprint, name: device.name });
	// Stable identity: the arm card clears the binding from an effect keyed on it.
	const clearBound = useCallback(() => setArmFor(null), []);
	// The approve dialog asks for the console password too. While it is open the arm and Moonlight
	// fields are disabled, so the dialog's is the only one a password manager can fill.
	const [approveOpen, setApproveOpen] = useState(false);
	// Moonlight/GameStream pairing only works when the host runs the compat planes (`--gamestream`,
	// off by default). Otherwise a Moonlight PIN can never arrive, so the card is dead UI — hide it
	// (and until host info loads, to avoid a flash of an un-actionable card).
	const host = useGetHostInfo();
	const gamestream = host.data?.gamestream === true;
	return (
		<PairingView
			pending={
				<PendingDevicesSection
					onArmFor={bindTo}
					onApproveOpenChange={setApproveOpen}
				/>
			}
			native={
				<NativePairingSection
					boundTo={armFor}
					onClearBound={clearBound}
					passwordLocked={approveOpen}
				/>
			}
			moonlight={
				gamestream ? (
					<MoonlightPairingSection passwordLocked={approveOpen} />
				) : null
			}
			paired={<PairedDevicesSection />}
		/>
	);
};
