import type { Meta, StoryObj } from "@storybook/react-vite";
import type { HostCheck } from "@/api/gen/model/hostCheck";
import type { RuntimeStatus } from "@/api/gen/model/runtimeStatus";
import type { AvatarProfile } from "@/components/profile-avatar";
import { RecentSessionsCard } from "@/sections/Home";
import { AttentionStrip } from "@/sections/Home/Attention";
import {
	GameRowView,
	SeatRowView,
	SessionRowView,
} from "@/sections/Home/NowRows";
import { HomeView } from "@/sections/Home/view";
import {
	lastSession,
	profilesWindows,
	statusActive,
	statusGrace,
	statusIdle,
} from "./lib/fixtures";
import { Routed } from "./lib/routed";

const noop = () => {};
const actions = {
	onStop: noop,
	onIdr: noop,
	onMute: noop,
	onAccess: noop,
	onPlayer: noop,
	busy: false,
};
const loaded = (data: RuntimeStatus) => ({
	data,
	isLoading: false,
	error: null,
});

/** The box's rows, the way `index.tsx` lays them out. */
const boxRows = (s: RuntimeStatus, named: AvatarProfile[] = []) =>
	s.sessions.map((row) => (
		<SessionRowView
			key={row.id}
			row={row}
			profile={named.find((p) => p.id === row.profile?.id) ?? undefined}
			game={s.games.find((g) => g.session_id === row.id)}
			stream={row.id === s.session_id ? s.stream : undefined}
			info={row.id === s.session_id ? s.session : undefined}
			sharedWith={[]}
			{...actions}
		/>
	));

const leon = profilesWindows.find((p) => p.display_name === "Leon");
const mia = profilesWindows.find((p) => p.display_name === "Mia");

const meta = {
	title: "Pages/Home",
	component: HomeView,
	decorators: [
		(Story) => (
			<Routed>
				<Story />
			</Routed>
		),
	],
	args: {
		status: loaded(statusActive),
		live: true,
		now: boxRows(statusActive),
		sessions: <RecentSessionsCard sessions={[lastSession]} />,
	},
} satisfies Meta<typeof HomeView>;

export default meta;
type Story = StoryObj<typeof meta>;

export const ActiveSession: Story = {};

/**
 * A box with seats: each row names its player, a full seat streams on its own desktop, and a seat
 * that is starting shows where the operator waits for it.
 */
export const SessionsWithProfiles: Story = {
	args: {
		now: (
			<>
				{boxRows(statusActive, profilesWindows)}
				{leon && (
					<SeatRowView
						profile={leon}
						occupant={leon.seat?.occupant}
						facts="Own desktop"
						action={{ label: "End session", onClick: noop, busy: false }}
					/>
				)}
				{mia && (
					<SeatRowView
						profile={mia}
						starting
						facts="Getting ready…"
						action={{ label: "Stop", onClick: noop, busy: false }}
					/>
				)}
			</>
		),
	},
};

/** Nothing live: the last session in one line. */
export const Idle: Story = {
	args: { status: loaded(statusIdle), live: false, now: null },
};

/** A game whose client vanished: the host closes it when the countdown runs out. */
export const GameWaitingForItsClient: Story = {
	args: {
		status: loaded(statusGrace),
		now: statusGrace.games.map((g) => (
			<GameRowView key={g.title} game={g} onEnd={noop} isEnding={false} />
		)),
	},
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
		id: "uinput_access",
		status: "ok",
		severity: "info",
		summary: "The input device nodes are reachable.",
		impact: "",
		params: {},
		source: "startup",
	},
];

/** Attention: a waiting device, an audio endpoint that is not ready, and a failed check. */
export const HostNeedsAttention: Story = {
	args: {
		status: loaded(statusIdle),
		live: false,
		now: null,
		attention: (
			<AttentionStrip
				checks={PROBLEMS}
				waiting={["Pixel 8"]}
				audio={{
					readiness: "mic_only",
					last_resort: false,
					mic_withheld: false,
				}}
			/>
		),
	},
};
