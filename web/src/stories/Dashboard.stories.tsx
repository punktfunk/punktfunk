import type { Meta, StoryObj } from "@storybook/react-vite";
import type { HostCheck } from "@/api/gen/model/hostCheck";
import { AttentionStrip } from "@/sections/Dashboard/AttentionCard";
import { DashboardView } from "@/sections/Dashboard/view";
import {
	profilesEvery,
	statusActive,
	statusGrace,
	statusIdle,
} from "./lib/fixtures";

const meta = {
	title: "Pages/Dashboard",
	component: DashboardView,
	args: {
		onStopSession: () => {},
		onRequestIdr: () => {},
		onEndGame: () => {},
		onStopOne: () => {},
		onIdrOne: () => {},
		onMuteOne: () => {},
		onAccessOne: () => {},
		onPlayerOne: () => {},
		isStopping: false,
		isRequestingIdr: false,
		isEndingGame: false,
		isChangingSession: false,
	},
} satisfies Meta<typeof DashboardView>;

export default meta;
type Story = StoryObj<typeof meta>;

export const ActiveSession: Story = {
	args: { status: { data: statusActive, isLoading: false, error: null } },
};

/** A box with profiles: each session row leads with its player's picture and name. */
export const SessionsWithProfiles: Story = {
	args: {
		status: { data: statusActive, isLoading: false, error: null },
		profiles: profilesEvery,
	},
};

export const Idle: Story = {
	args: { status: { data: statusIdle, isLoading: false, error: null } },
};

/** A game whose client vanished: the host closes it when the countdown runs out. */
export const GameWaitingForItsClient: Story = {
	args: { status: { data: statusGrace, isLoading: false, error: null } },
};

const PROBLEMS: HostCheck[] = [
	{
		id: "takeover_privilege",
		status: "fail",
		severity: "critical",
		summary: "User “enrico” is not in the “punktfunk” group",
		impact: "Every takeover degrades to mirroring this machine's own session.",
		params: {},
		source: "startup",
	},
	{
		id: "virtual_deck_vhci",
		status: "fail",
		severity: "warning",
		summary: "The group “punktfunk” was granted but this session predates it",
		impact: "The virtual Steam Deck controller cannot attach.",
		params: {},
		source: "startup",
	},
	{
		id: "uinput_access",
		status: "ok",
		severity: "info",
		summary: "The input device nodes are reachable.",
		impact: "",
		params: {},
		source: "startup",
	},
];

/**
 * The attention strip in place: worst-first, one line each, no remedies — the dashboard points at
 * the troubleshooting page rather than becoming a manual. The healthy counterpart is every OTHER
 * story on this page: they pass no `attention` at all, which is exactly what a healthy host renders.
 */
export const HostNeedsAttention: Story = {
	args: {
		status: { data: statusIdle, isLoading: false, error: null },
		attention: <AttentionStrip checks={PROBLEMS} />,
	},
};
