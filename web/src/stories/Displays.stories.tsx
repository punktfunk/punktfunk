import type { Meta, StoryObj } from "@storybook/react-vite";
import type { ApiDisplayInfo, ApiMonitorInfo } from "@/api/gen/model";
import { DesktopMap, ghostBox } from "@/sections/Displays/DesktopMap";
import { describePolicy } from "@/sections/Displays/describePolicy";
import { ScreenRows } from "@/sections/Displays/ScreenRows";
import { displayEffective } from "./lib/fixtures";

/**
 * The **desktop map** (design/web-console-overhaul.md §5.4) — the page's answer to "what
 * happens to my screens when a device connects", and the only part of the rebuilt Displays page
 * whose correctness is geometric rather than textual. `describePolicy` is covered by its own
 * table test; the sentence is rendered here beside the map because the two are read together,
 * and a story is the only place a reviewer sees them that way.
 *
 * The old story for this page existed to pin the motion nesting of a preset grid that no longer
 * exists — the presets moved into a dialog, so the grid, its stagger and the tab shell it hung
 * from all went with the draft machinery.
 */
const mon = (over: Partial<ApiMonitorInfo>): ApiMonitorInfo => ({
	connector: "DP-1",
	description: "Dell U2718Q",
	enabled: true,
	managed: false,
	mode: "2560x1440@120",
	primary: false,
	scale: 1,
	selected: false,
	x: 0,
	y: 0,
	...over,
});

const disp = (over: Partial<ApiDisplayInfo>): ApiDisplayInfo => ({
	backend: "kwin",
	display_index: 0,
	group: 0,
	mode: "3840x2160@120",
	sessions: 1,
	slot: 1,
	state: "active",
	topology: "extend",
	// auto-row places a streamed screen beyond the right edge of the desk, not on top of it.
	x: 4480,
	y: 0,
	...over,
});

const MONITORS = [
	mon({ primary: true }),
	mon({
		connector: "HDMI-1",
		description: "LG TV",
		mode: "1920x1080@60",
		x: 2560,
	}),
];

const Harness = ({
	monitors = MONITORS,
	displays = [],
	dimMonitors = false,
	live = false,
}: {
	monitors?: ApiMonitorInfo[];
	displays?: ApiDisplayInfo[];
	dimMonitors?: boolean;
	live?: boolean;
}) => (
	<div className="max-w-3xl space-y-4">
		<DesktopMap
			monitors={monitors}
			displays={displays}
			dimMonitors={dimMonitors}
			// The idle map's ghost: where the next device's screen lands under Extend.
			ghost={ghostBox(monitors, dimMonitors ? "exclusive" : "extend", false)}
			onMove={() => {}}
		/>
		<ScreenRows
			monitors={monitors}
			displays={displays}
			pinned={null}
			pinSupported
			effective={{
				...displayEffective,
				topology: dimMonitors ? "exclusive" : "extend",
			}}
			overlaid={[]}
			onPick={() => {}}
			onKeepLit={() => {}}
			onRelease={() => {}}
		/>
		<p className="text-sm">{describePolicy(displayEffective, { live })}</p>
	</div>
);

const meta = {
	title: "Pages/Displays",
	component: Harness,
} satisfies Meta<typeof Harness>;

export default meta;
type Story = StoryObj<typeof meta>;

/** An idle host: the monitors, the ghost where the next device lands, and the sentence. */
export const Idle: Story = {};

/** One device streaming beside the desk — the shape the default preset produces. */
export const Streaming: Story = {
	args: {
		displays: [disp({ client: "Living-room TV", identity_slot: 1 })],
		live: true,
	},
};

/**
 * A kept screen's row carries Release; an active one does not — tearing that down is session
 * control, not display management. The map only shows state.
 */
export const KeptAndActive: Story = {
	args: {
		displays: [
			disp({ client: "Living-room TV", identity_slot: 1 }),
			disp({
				slot: 2,
				client: "Enrico's iPad",
				mode: "2560x1600@120",
				state: "lingering",
				expires_in_ms: 8_000,
				identity_slot: 2,
				x: 8320,
				y: 0,
			}),
		],
	},
};

/**
 * Workstation on a box three devices used in the last minutes: each got an exclusive screen of
 * its own, and every group reports itself at (0, 0). The live one sits over the desk; the kept
 * ones stand apart instead of stacking on it.
 */
export const SeparateDesktops: Story = {
	args: {
		monitors: [mon({ primary: true })],
		displays: [
			disp({
				slot: 1,
				group: 0,
				client: "Enrico's iPad",
				mode: "2560x1600@120",
				state: "lingering",
				expires_in_ms: 120_000,
				topology: "exclusive",
				x: 0,
			}),
			disp({
				slot: 2,
				group: 1,
				client: "Mac Studio",
				mode: "2560x1440@120",
				topology: "exclusive",
				x: 0,
			}),
			disp({
				slot: 3,
				group: 2,
				client: "Steam Deck",
				mode: "1280x800@90",
				state: "lingering",
				expires_in_ms: 240_000,
				topology: "exclusive",
				x: 0,
			}),
		],
		dimMonitors: true,
	},
};

/** Hovering the Exclusive preset: the physical monitors dim, which IS the preview (§5.3). */
export const ExclusivePreview: Story = {
	args: {
		displays: [disp({ client: "Living-room TV", identity_slot: 1 })],
		dimMonitors: true,
	},
};

/**
 * Odd geometry is the map's stated risk (§10): a negative origin, and a 5K panel beside a
 * 1080p one. Fit-to-box has to keep both legible rather than scaling to the largest.
 */
export const OddGeometry: Story = {
	args: {
		monitors: [
			mon({ connector: "DP-3", mode: "5120x2880@60", x: -5120, primary: true }),
			mon({ connector: "HDMI-2", mode: "1920x1080@60", x: 0, y: 1800 }),
		],
		displays: [
			disp({ client: "Work laptop", identity_slot: 4, x: 1920, y: 1800 }),
		],
	},
};
