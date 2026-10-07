import type { Meta, StoryObj } from "@storybook/react-vite";
import type { HostCheck } from "@/api/gen/model/hostCheck";
import { ChecksCard } from "@/sections/Logs/ChecksCard";

/**
 * The troubleshooting page's checks list.
 *
 * The states that matter here are the ones that are easy to get wrong and impossible to see in a
 * diff: a healthy host must not grow chrome, an `inapplicable` row must stay out of the way without
 * disappearing, and a check this console has never heard of must still render — because host N and
 * console N−1 is a supported pairing, and the host always sends readable English text with it.
 */

const check = (
	over: Partial<HostCheck> & Pick<HostCheck, "id">,
): HostCheck => ({
	status: "ok",
	severity: "info",
	summary: "",
	impact: "",
	params: {},
	source: "startup",
	...over,
});

const TAKEOVER_FAIL = check({
	id: "takeover_privilege",
	status: "fail",
	severity: "critical",
	summary: "User “enrico” is not in the “punktfunk” group",
	impact:
		"Streams that need the managed takeover cannot stop sddm.service, so every one of them degrades to mirroring this machine's own session instead. With the panel off that looks like a black screen on every connect, and nothing else reports it.",
	remedy: {
		text: "Add the user to the “punktfunk” group, then restart the computer. The same group gates the virtual Steam Deck pad's usbip nodes, which can present arbitrary emulated USB devices — join it only on a machine you trust.",
		command: "sudo usermod -aG punktfunk enrico",
		relogin_required: true,
	},
	params: { user: "enrico", group: "punktfunk", dm: "sddm.service" },
	since_unix: 1_750_000_000,
});

/** The "I already added myself!" state — a re-login, not another usermod. */
const VHCI_RELOGIN = check({
	id: "virtual_deck_vhci",
	status: "fail",
	severity: "warning",
	summary: "The group “punktfunk” was granted but this session predates it",
	impact:
		"The virtual Steam Deck controller cannot attach, so Steam Input never sees it — in Game Mode that means nothing can be navigated with a pad.",
	remedy: {
		text: "Restart the computer. The membership is already recorded, but the host's services started before it and keep their old groups until a restart — logging out is not enough.",
		relogin_required: true,
	},
	params: { group: "punktfunk", user: "enrico" },
});

const UINPUT_OK = check({
	id: "uinput_access",
	summary: "The input device nodes are reachable.",
});

const CONFLICT_OK = check({
	id: "server_conflict",
	summary: "No other game-streaming server is active on this machine.",
});

const TAKEOVER_NA = check({
	id: "takeover_privilege",
	status: "inapplicable",
	summary:
		"No display manager drives this machine's logins, so a takeover has nothing to stop.",
});

/** A host newer than this console: the id is unknown, so only the wire text can be shown. */
const UNKNOWN = check({
	id: "thermal_throttling",
	status: "warn",
	severity: "warning",
	summary: "The encoder GPU has been thermally throttled for 4 minutes",
	impact:
		"Frames are being dropped under load, which reads as stutter on the client.",
	remedy: {
		text: "Check this machine's airflow and fan curve.",
		relogin_required: false,
	},
});

const meta = {
	title: "Pages/Troubleshooting",
	component: ChecksCard,
	args: { onRerun: () => {}, isRerunning: false },
} satisfies Meta<typeof ChecksCard>;

export default meta;
type Story = StoryObj<typeof meta>;

/** The common case: nothing is wrong, and the list says so without shouting. */
export const Healthy: Story = {
	args: { checks: [UINPUT_OK, CONFLICT_OK] },
};

/** The `.181` defect this whole feature exists for. */
export const OneCritical: Story = {
	args: { checks: [TAKEOVER_FAIL, UINPUT_OK, CONFLICT_OK] },
};

/** Two problems of different severity, plus a row that does not apply to this box. */
export const Mixed: Story = {
	args: { checks: [TAKEOVER_FAIL, VHCI_RELOGIN, UINPUT_OK, TAKEOVER_NA] },
};

/**
 * Console N paired with host N+1. The check has no localized name and no localized prose here, and
 * it still has to be readable — that is what the host's English fallback text is for.
 */
export const UnknownCheckFromANewerHost: Story = {
	args: { checks: [UNKNOWN, UINPUT_OK] },
};

/** An older host has no diagnostics route at all. Not an error — just nothing to report. */
export const HostTooOld: Story = {
	args: { checks: [], unsupported: true },
};
