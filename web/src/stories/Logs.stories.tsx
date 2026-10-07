import type { Meta, StoryObj } from "@storybook/react-vite";
import { Download } from "lucide-react";
import type { ClientLogMeta } from "@/api/gen/model/clientLogMeta";
import type { LogEntry } from "@/api/gen/model/logEntry";
import { Button } from "@/components/ui/button";
import { LogsCard } from "@/sections/Diagnostics/Logs/LogsCard";
import {
	bundleRows,
	deviceSource,
	HOST_SOURCE,
	hostRows,
	mergeRows,
	PLUGINS_SOURCE,
} from "@/sections/Diagnostics/Logs/rows";
import { LogsView } from "@/sections/Diagnostics/Logs/view";

const noop = () => {};

// A deterministic slice of host logs covering every level, incl. the gamepad-driver health lines
// the page exists to surface — no live host needed.
const BASE = 1_750_000_000_000;
const entry = (
	seq: number,
	level: string,
	target: string,
	msg: string,
): LogEntry => ({ seq, ts_ms: BASE + seq * 750, level, target, msg });

const fixtureEntries: LogEntry[] = [
	entry(
		1,
		"INFO",
		"punktfunk_host",
		"punktfunk-host 0.4.2 (punktfunk_core ABI v2)",
	),
	entry(
		2,
		"INFO",
		"punktfunk_host::mgmt",
		"management API listening over HTTPS addr=0.0.0.0:47990",
	),
	entry(
		3,
		"DEBUG",
		"punktfunk_host::discovery",
		"mDNS advertise _punktfunk._udp pair=required",
	),
	entry(
		4,
		"INFO",
		"punktfunk_host::punktfunk1",
		"session start mode=1920x1080@60 codec=hevc",
	),
	entry(
		5,
		"INFO",
		"punktfunk_host::inject",
		"virtual Xbox 360 created (Windows XUSB companion)",
	),
	entry(
		6,
		"WARN",
		"punktfunk_host::inject",
		"gamepad driver not attached to Global\\pfxusb-shm-0 after 3s — is the pf_xusb driver installed? (punktfunk-host.exe driver install --gamepad)",
	),
	entry(
		7,
		"ERROR",
		"punktfunk_host::inject",
		"virtual Xbox 360 creation failed — controller input disabled (is the pf_xusb driver installed?)",
	),
	entry(
		8,
		"INFO",
		"punktfunk_host::encode",
		"NVENC opened 1920x1080 nv12 gop=inf rfi=on",
	),
	// Lines the plugin runner shipped up (`POST /api/v1/plugins/logs`), targeted `plugin:<name>`.
	// They share the ring and the cursor with the host's own, which is what the Host/Plugins
	// filter exists to separate — so the fixture has to carry both to be worth screenshotting.
	entry(9, "INFO", "plugin:runner", "starting virtualhere"),
	entry(
		10,
		"INFO",
		"plugin:virtualhere",
		"bound couch-deck.11 (Thrustmaster T300RS) for stream",
	),
	entry(
		11,
		"ERROR",
		"plugin:virtualhere",
		"vhclientx86_64 failed (ETIMEDOUT) — the VirtualHere client is not answering on /tmp/vhclient",
	),
];

const DECK: ClientLogMeta = {
	id: "1750000000_ab12cd34ef567890_couch-deck",
	device_name: "couch-deck",
	fingerprint_prefix: "ab12cd34ef567890",
	received_ms: BASE + 12_000,
	size_bytes: 384_512,
};

/**
 * A bundle exactly as `clients/session`'s ring layer writes it: a header line that does NOT parse,
 * then `<ISO8601-Z> <LEVEL> <target> <msg>`. The unparsed header is in the fixture on purpose — it
 * is the cheapest standing proof that the fail-soft path renders rather than swallows, which is
 * what the Apple/Android/webOS legs will depend on when they land with formats of their own.
 *
 * Its timestamps interleave with the host's rather than sitting after them, because interleaving is
 * the entire reason the two are in one pane.
 */
const iso = (seq: number) => new Date(BASE + seq * 750).toISOString();
const deckBundle = [
	"punktfunk-session 0.4.2 (linux x86_64)",
	`${iso(4)} INFO  punktfunk_session::stream connected host=skynet mode=1920x1080@60`,
	`${iso(6)} WARN  punktfunk_session::audio egress late=31% — link stalled`,
	`${iso(7)} ERROR punktfunk_session::pad no rumble device for DualSense (permission denied)`,
	`${iso(9)} INFO  punktfunk_session::stream decode queue drained`,
].join("\n");

const hostOnly = hostRows(fixtureEntries);
const merged = mergeRows([hostOnly, bundleRows(deckBundle, DECK)]);

const chip = (id: string, label: string, selected: boolean, hint?: string) => ({
	id,
	label,
	selected,
	hint,
});

const meta = {
	title: "Pages/Logs",
	component: LogsView,
	parameters: { layout: "padded" },
} satisfies Meta<typeof LogsView>;

export default meta;
type Story = StoryObj<typeof meta>;

// The real page layout (LogsView) with the pure viewer card + fixture entries in its slot.
// `shareMode` is probed from the browser on the live page; the stories pin one of each so both the
// desktop (clipboard) and mobile (share sheet) affordance stay covered by the screenshot run.
export const Following: Story = {
	args: {
		// The page-level export lives in the heading, deliberately away from the toolbar's own
		// download ("what I am looking at"). Pinned in a story so the two cannot drift back together.
		actions: (
			<Button variant="outline">
				<Download className="size-4" />
				Export all
			</Button>
		),
		viewer: (
			<LogsCard
				rows={hostOnly}
				sources={[
					chip(HOST_SOURCE, "Host", true),
					chip(PLUGINS_SOURCE, "Plugins", true),
				]}
				onToggleSource={noop}
				devicesEmpty
				follow
				onFollow={noop}
				onClear={noop}
				onDownload={noop}
				onShare={noop}
				shareMode="copy"
				dropped={false}
			/>
		),
	},
};

export const PausedWithGap: Story = {
	args: {
		viewer: (
			<LogsCard
				rows={hostOnly}
				sources={[
					chip(HOST_SOURCE, "Host", true),
					chip(PLUGINS_SOURCE, "Plugins", true),
				]}
				onToggleSource={noop}
				devicesEmpty
				follow={false}
				onFollow={noop}
				onClear={noop}
				onDownload={noop}
				onShare={noop}
				shareMode="share"
				dropped
			/>
		),
	},
};

/**
 * The view the multi-select exists for: the host and one device on one timeline, device lines
 * carrying their origin tag. This is the story to check when touching the merge, the tag, or the
 * chips — a regression here is invisible in the host-only stories above.
 */
export const HostAndDeviceMerged: Story = {
	args: {
		viewer: (
			<LogsCard
				rows={merged}
				sources={[
					chip(HOST_SOURCE, "Host", true),
					chip(PLUGINS_SOURCE, "Plugins", true),
					chip(deviceSource(DECK.id), DECK.device_name, true, "12:04"),
				]}
				onToggleSource={noop}
				follow={false}
				onFollow={noop}
				onClear={noop}
				onDownload={noop}
				onShare={noop}
				shareMode="copy"
				dropped={false}
			/>
		),
	},
};

/** A device chip mid-fetch, and one whose bundle failed — both states live on the chip itself. */
export const DeviceChipStates: Story = {
	args: {
		viewer: (
			<LogsCard
				rows={hostOnly}
				sources={[
					chip(HOST_SOURCE, "Host", true),
					chip(PLUGINS_SOURCE, "Plugins", false),
					{
						...chip(deviceSource(DECK.id), DECK.device_name, false, "12:04"),
						state: "loading" as const,
					},
					{
						...chip(deviceSource("other"), "living-room-tv", false, "09:41"),
						state: "error" as const,
					},
				]}
				onToggleSource={noop}
				follow={false}
				onFollow={noop}
				onClear={noop}
				onDownload={noop}
				onShare={noop}
				shareMode="copy"
				dropped={false}
			/>
		),
	},
};
