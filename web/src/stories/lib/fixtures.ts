// Mock API payloads for the page stories — typed against the generated models so
// they stay honest if the OpenAPI schema changes.
import type { AvailableCompositor } from "@/api/gen/model/availableCompositor";
import type { Capture } from "@/api/gen/model/capture";
import type { CaptureMeta } from "@/api/gen/model/captureMeta";
import type { CustomPreset } from "@/api/gen/model/customPreset";
import type { DisplayPolicy } from "@/api/gen/model/displayPolicy";
import type { EffectivePolicy } from "@/api/gen/model/effectivePolicy";
import type { GameEntry } from "@/api/gen/model/gameEntry";
import type { HostInfo } from "@/api/gen/model/hostInfo";
import type { NativeClient } from "@/api/gen/model/nativeClient";
import type { NativePairStatus } from "@/api/gen/model/nativePairStatus";
import type { PairedClient } from "@/api/gen/model/pairedClient";
import type { PairingStatus } from "@/api/gen/model/pairingStatus";
import type { PendingDevice } from "@/api/gen/model/pendingDevice";
import type { ProfileAdmin } from "@/api/gen/model/profileAdmin";
import type { RuntimeStatus } from "@/api/gen/model/runtimeStatus";
import type { Seating } from "@/api/gen/model/seating";
import type { StatsSample } from "@/api/gen/model/statsSample";
import type { StatsStatus } from "@/api/gen/model/statsStatus";

export const hostInfo: HostInfo = {
	abi_version: 2,
	app_version: "7.1.450.0",
	codecs: ["h264", "hevc", "av1"],
	door: false,
	gamestream: true,
	gfe_version: "3.23.0.74",
	hostname: "ENRICOS-DESKTOP",
	local_ip: "192.168.1.173",
	os: "linux/fedora/bazzite",
	os_name: "Bazzite 42 (Kinoite)",
	ports: {
		audio: 48000,
		control: 47999,
		http: 47989,
		https: 47984,
		mgmt: 47990,
		rtsp: 48010,
		video: 47998,
	},
	uniqueid: "0f8a1c3e9b7d4a62",
	version: "0.2.0",
};

export const compositors: AvailableCompositor[] = [
	{ id: "kwin", label: "KWin (Plasma)", available: true, default: true },
	{ id: "gamescope", label: "gamescope", available: true, default: false },
	{ id: "mutter", label: "Mutter (GNOME)", available: false, default: false },
	{ id: "wlroots", label: "Sway / wlroots", available: false, default: false },
];

export const statusActive: RuntimeStatus = {
	video_streaming: true,
	audio_streaming: true,
	paired_clients: 3,
	native_paired_clients: 2,
	pin_pending: false,
	active_sessions: 2,
	// Two clients on one display: the pair the per-session controls exist for.
	sessions: [
		{
			id: 1,
			plane: "native",
			client: "aabbccddeeff",
			client_name: "Living room TV",
			profile: { id: "9a3f1c2b7e40", display_name: "Kid" },
			mode: "5120x1440@240",
			hdr: true,
			join: false,
			muted: false,
			access_level: "full",
			// Player 1 by the pick, so the co-op pair below reads the way it would on glass.
			pads: [0],
			preferred_pad_slot: 0,
			uptime_s: 4_512,
		},
		{
			id: 2,
			plane: "native",
			client: "112233445566",
			client_name: "Enrico's phone",
			profile: { id: "4f1c3a9b0e27", display_name: "Enrico" },
			mode: "5120x1440@240",
			hdr: false,
			join: true,
			muted: true,
			access_level: "view",
			pads: [1],
			uptime_s: 96,
		},
	],
	session_id: 1,
	session: { width: 5120, height: 1440, fps: 240 },
	stream: {
		codec: "hevc",
		width: 5120,
		height: 1440,
		fps: 240,
		bitrate_kbps: 150_000,
		min_fec: 5,
		packet_size: 1392,
	},
	games: [
		{
			session_id: 1,
			client: "Living room TV",
			app_id: "steam:1145360",
			title: "Hades",
			store: "steam",
			plane: "native",
			state: "running",
		},
	],
};

export const statusIdle: RuntimeStatus = {
	video_streaming: false,
	audio_streaming: false,
	paired_clients: 1,
	native_paired_clients: 0,
	pin_pending: true,
	active_sessions: 0,
	sessions: [],
	session: null,
	stream: null,
	games: [],
};

/**
 * Idle, but a game its client walked away from is still running — on a countdown to being closed.
 * The state the running-game card exists for.
 */
export const statusGrace: RuntimeStatus = {
	...statusIdle,
	games: [
		{
			client: "Living room TV",
			app_id: "steam:1145360",
			title: "Hades",
			store: "steam",
			plane: "native",
			state: "grace",
			grace_remaining_s: 252,
		},
	],
};

export const pairedClients: PairedClient[] = [
	{
		fingerprint:
			"a1b2c3d4e5f60718293a4b5c6d7e8f90112233445566778899aabbccddeeff00",
		subject: "enricos-macbook",
		not_before_unix: 1_718_000_000,
		not_after_unix: 2_030_000_000,
	},
	{
		fingerprint:
			"ff00eeddccbbaa998877665544332211009f8e7d6c5b4a39281706f5e4d3c2b1",
		subject: "living-room-tv",
		// Named by the operator — the row that shows what a rename buys you next to a sibling that
		// still reads as its (identical-for-everyone) certificate subject.
		label: "Living Room TV",
		not_before_unix: 1_718_500_000,
		not_after_unix: 2_030_000_000,
	},
	{
		fingerprint:
			"0011223344556677889900aabbccddeeff112233445566778899aabbccddeeff",
		subject: null,
	},
];

const noArt = { header: null, hero: null, logo: null, portrait: null };
export const library: GameEntry[] = [
	{
		id: "steam:1245620",
		store: "steam",
		title: "Elden Ring",
		art: noArt,
		launch: null,
	},
	{
		id: "steam:1086940",
		store: "steam",
		title: "Baldur's Gate 3",
		art: noArt,
		launch: null,
	},
	{
		id: "steam:413150",
		store: "steam",
		title: "Stardew Valley",
		art: noArt,
		launch: null,
	},
	{
		id: "custom:retroarch",
		store: "custom",
		title: "RetroArch",
		art: noArt,
		launch: null,
	},
	// An emulated title with the metadata fields filled — exercises the platform
	// badge (non-PC) and the year in the caption.
	{
		id: "custom:sotc",
		store: "custom",
		title: "Shadow of the Colossus",
		art: noArt,
		launch: null,
		platform: "PS2",
		developer: "Team Ico",
		publisher: "Sony Computer Entertainment",
		release_year: 2005,
		genres: ["Adventure"],
		tags: ["favorite"],
		region: "PAL",
		players: 1,
	},
];

// --- Performance (stats) page ------------------------------------------------

export const statsStatusIdle: StatsStatus = {
	armed: false,
	kind: "native",
	sample_count: 0,
	started_unix_ms: 0,
	elapsed_ms: 0,
};

// A Linux native pipeline: queue → capture → submit → encode → send. Deterministic (no
// Math.random) so the screenshot is byte-stable across CI runs; a gentle sine gives the charts
// a realistic shape without a live capture.
const STAGE_BASE_US: Record<string, number> = {
	queue: 180,
	capture: 320,
	submit: 90,
	encode: 760,
	send: 140,
};
const STAGE_ORDER = ["queue", "capture", "submit", "encode", "send"];

function buildSamples(n: number): StatsSample[] {
	const out: StatsSample[] = [];
	for (let i = 0; i < n; i++) {
		const wobble = Math.sin(i / 4);
		const stages = STAGE_ORDER.map((name) => {
			const base = STAGE_BASE_US[name] ?? 100;
			const p50 = Math.round(base + wobble * base * 0.15);
			return { name, p50_us: p50, p99_us: Math.round(p50 * 1.8) };
		});
		const host = stages.reduce((sum, st) => sum + st.p50_us, 0) + 220;
		out.push({
			t_ms: i * 1000,
			session_id: 1,
			fps: 240,
			repeat_fps: i % 3 === 0 ? 2 : 1,
			mbps: 920 + wobble * 55,
			bitrate_kbps: 150_000,
			// The host measures its own send drops; loss and FEC are the client's to count.
			send_dropped: i % 23 === 0 ? 1 : 0,
			host_p50_us: host,
			host_p99_us: Math.round(host * 1.7),
			rtt_us: Math.round(900 + wobble * 150),
			fec_us: 45 + wobble * 5,
			seal_us: 140 + wobble * 12,
			sock_us: 300 + wobble * 40,
			stages,
		});
	}
	return out;
}

// The Windows driver path at 120 Hz: the driver's lump sits just under one 8.3 ms frame, and
// the pool drops a frame now and then.
function buildDriverSamples(n: number): StatsSample[] {
	const out: StatsSample[] = [];
	for (let i = 0; i < n; i++) {
		const wobble = Math.sin(i / 3);
		const driver = Math.round(7_400 + wobble * 600);
		const stages = [
			{ name: "driver", p50_us: driver, p99_us: Math.round(driver * 1.25) },
			{ name: "copy", p50_us: 180, p99_us: 260 },
			{ name: "send", p50_us: 240, p99_us: 410 },
		];
		const host = driver + 180 + 240 + 90;
		out.push({
			t_ms: i * 2000,
			session_id: 1,
			fps: 118 + (i % 4 === 0 ? -6 : 0),
			repeat_fps: 0,
			mbps: 64 + wobble * 6,
			bitrate_kbps: 80_000,
			frames_dropped: i % 11 === 0 ? 2 : 0,
			send_dropped: 0,
			host_p50_us: host,
			host_p99_us: Math.round(host * 1.3),
			rtt_us: Math.round(1_600 + wobble * 300),
			stages,
		});
	}
	return out;
}

export const captureMetas: CaptureMeta[] = [
	{
		id: "cap-20260628-2041",
		client: "enricos-macbook",
		kind: "native",
		codec: "hevc",
		width: 5120,
		height: 1440,
		fps: 240,
		duration_ms: 92_000,
		sample_count: 92,
		started_unix_ms: 1_782_415_260_000,
		encoder_backend: "nvenc",
		gpu: "NVIDIA GeForce RTX 4090",
	},
	{
		id: "cap-20260628-1955",
		client: "living-room-pc",
		kind: "native",
		codec: "hevc",
		width: 3840,
		height: 2160,
		fps: 120,
		duration_ms: 7_200_000,
		sample_count: 5400,
		started_unix_ms: 1_782_412_500_000,
		encoder_backend: "driver-nvenc",
		gpu: "NVIDIA GeForce RTX 4090",
		truncated: true,
	},
	{
		id: "cap-20260628-1903",
		client: "living-room-tv",
		kind: "gamestream",
		codec: "av1",
		width: 3840,
		height: 2160,
		fps: 120,
		duration_ms: 240_000,
		sample_count: 240,
		started_unix_ms: 1_782_409_380_000,
	},
];

export const captureDetail: Capture = {
	meta: captureMetas[0] as CaptureMeta,
	samples: buildSamples(60),
};

export const captureDetailDriver: Capture = {
	meta: captureMetas[1] as CaptureMeta,
	samples: buildDriverSamples(60),
};

// --- Pairing page ------------------------------------------------------------

export const nativePairArmed: NativePairStatus = {
	enabled: true,
	armed: true,
	pin: "4827",
	expires_in_secs: 98,
	paired_clients: 2,
};

/** Fixed "now" for every access countdown in the stories — screenshots must not drift. */
export const accessNowUnix = 1_755_200_000;

export const pendingDevices: PendingDevice[] = [
	{
		id: 1,
		name: "studio-deck",
		fingerprint:
			"9f8e7d6c5b4a39281706f5e4d3c2b1a0998877665544332211ffeeddccbbaa00",
		age_secs: 8,
		source: "lan",
		until_disconnect: false,
	},
	{
		id: 2,
		name: "Mac Mini",
		fingerprint:
			"ff00eeddccbbaa998877665544332211009f8e7d6c5b4a39281706f5e4d3c2b1",
		age_secs: 30,
		source: "lan",
		until_disconnect: false,
	},
	// A knock from the internet: badged, and offered an "Arm PIN" instead of Approve.
	{
		id: 4,
		name: "Friend's Deck",
		fingerprint:
			"5c5b5a595857565554535251504f4e4d4c4b4a494847464544434241403f3e3d",
		age_secs: 3,
		source: "wan",
		until_disconnect: false,
	},
];

/** The expired-guest re-knock: already known, stored as Controller only · 4 h (now past). */
export const pendingGuestReknock: PendingDevice = {
	id: 3,
	name: "leons-deck",
	fingerprint:
		"0011223344556677889900aabbccddeeff102030405060708090a0b0c0d0e0f0",
	age_secs: 12,
	source: "lan",
	until_disconnect: false,
	access_level: "controller",
	grants: 0x01,
	granted_unix: accessNowUnix - 6 * 3600,
	expires_unix: accessNowUnix - 2 * 3600,
};

export const nativeClients: NativeClient[] = [
	{
		name: "enricos-macbook",
		fingerprint:
			"a1b2c3d4e5f60718293a4b5c6d7e8f90112233445566778899aabbccddeeff00",
		access_level: "full",
		grants: 0x3f,
		granted_unix: accessNowUnix - 30 * 86400,
		expires_unix: null,
		until_disconnect: false,
	},
	{
		name: "living-room-tv",
		fingerprint:
			"ff00eeddccbbaa998877665544332211009f8e7d6c5b4a39281706f5e4d3c2b1",
		access_level: "controller",
		grants: 0x01,
		granted_unix: accessNowUnix - 2 * 3600,
		// 1 h 58 min out — renders as the design's "Controller · 2 h left".
		expires_unix: accessNowUnix + 7080,
		until_disconnect: false,
	},
	// A guest admitted for the evening: no deadline, the record goes when they do.
	{
		name: "Friend's Deck",
		fingerprint:
			"5c5b5a595857565554535251504f4e4d4c4b4a494847464544434241403f3e3d",
		access_level: "controller",
		grants: 0x01,
		granted_unix: accessNowUnix - 900,
		expires_unix: null,
		until_disconnect: true,
	},
];

export const pairingIdle: PairingStatus = { pin_pending: false, pending: [] };

/** The six axes a preset expands to — the baseline the built-ins vary from. */
const policyFields = (
	over: Partial<EffectivePolicy> = {},
): EffectivePolicy => ({
	identity: "per-client",
	keep_alive: { mode: "duration", seconds: 300 },
	layout: { mode: "auto-row", positions: {} },
	max_displays: 4,
	mode_conflict: "separate",
	topology: "auto",
	...over,
});

/**
 * The built-in presets as `GET /display/settings` returns them. Summaries are the host's own prose
 * (it composes them from the fields), so they are literal strings here rather than i18n messages.
 */
export const displayPresets: {
	id: string;
	summary: string;
	fields: EffectivePolicy;
}[] = [
	{
		id: "default",
		summary:
			"A virtual display per client, released 5 minutes after it disconnects.",
		fields: policyFields(),
	},
	{
		id: "shared-desktop",
		summary:
			"Every client sees the same desktop — no extra displays are created.",
		fields: policyFields({
			identity: "shared",
			topology: "primary",
			mode_conflict: "join",
		}),
	},
	{
		id: "hotdesk",
		summary:
			"One display at a time; the physical heads go dark while you stream.",
		fields: policyFields({
			topology: "exclusive",
			max_displays: 1,
			keep_alive: { mode: "off" },
		}),
	},
	{
		id: "workstation",
		summary: "Adds a display beside the monitors already on the desk.",
		fields: policyFields({ topology: "extend" }),
	},
	{
		id: "gaming-rig",
		summary:
			"Pins the display so it survives every disconnect — free it with Release.",
		fields: policyFields({
			topology: "exclusive",
			keep_alive: { mode: "forever" },
		}),
	},
];

/** Two operator-saved bundles, so the custom-preset rail has something in it. */
export const displayCustomPresets: CustomPreset[] = [
	{
		id: "cp-couch",
		name: "Couch (TV only)",
		fields: policyFields({
			topology: "exclusive",
			max_displays: 1,
			keep_alive: { mode: "forever" },
		}),
		game_session: "dedicated",
	},
	{
		id: "cp-office",
		name: "Office desk",
		fields: policyFields({ topology: "extend", identity: "shared" }),
	},
];

/** What the host reports as in force — the `default` preset's expansion. */
export const displayEffective: EffectivePolicy = policyFields();

/** The stored policy: a plain built-in pick, which is what most hosts sit on. */
export const displayPolicy: DisplayPolicy = {
	preset: "default",
	game_session: "auto",
	version: 1,
};

/** A box with one person: the owner card alone, and the page's only control is Add profile. */
export const profilesOne: ProfileAdmin[] = [
	{
		id: "4f1c3a9b0e27",
		display_name: "Enrico",
		accent: "#3b82f6",
		owner: true,
		home: "desktop",
		last_used_unix: accessNowUnix - 3600,
		default: true,
	},
];

/** Every card state on Linux: the owner, a desktop sharer, and a light seat in each seat state. */
export const profilesEvery: ProfileAdmin[] = [
	...profilesOne,
	{
		id: "0b7d5e2a91c4",
		display_name: "Anna",
		accent: "#ec4899",
		owner: false,
		home: "desktop",
		last_used_unix: 0,
		default: false,
	},
	{
		id: "9a3f1c2b7e40",
		display_name: "Kid",
		accent: "#f97316",
		owner: false,
		home: "bigpicture",
		seat: {
			state: "ready",
			port: 9777,
			steam_sign_in: false,
			kind: "steam",
		},
		last_used_unix: accessNowUnix - 600,
		default: false,
		legacy_device: "ab12cd34",
	},
	{
		id: "2c4e6a8b0d1f",
		display_name: "Guest",
		owner: false,
		home: "bigpicture",
		seat: { state: "ready", port: 9777, steam_sign_in: true, kind: "steam" },
		last_used_unix: 0,
		default: false,
	},
	{
		id: "7e1f3b5d9a2c",
		display_name: "Leon",
		accent: "#22c55e",
		owner: false,
		home: "bigpicture",
		seat: {
			state: "occupied",
			port: 9777,
			occupant: "Leon's Deck",
			kind: "steam",
		},
		last_used_unix: accessNowUnix - 60,
		default: false,
	},
	{
		id: "5a7c9e1b3d5f",
		display_name: "Mia",
		accent: "#a855f7",
		owner: false,
		home: "bigpicture",
		seat: {
			state: "starting",
			port: 9777,
			detail: "Starting Steam",
			kind: "steam",
		},
		last_used_unix: accessNowUnix - 86_400,
		default: false,
	},
	{
		id: "3d5f7b9a1c3e",
		display_name: "Oma",
		accent: "#14b8a6",
		owner: false,
		home: "desktop",
		seat: { state: "stopped", port: 9777, kind: "steam" },
		last_used_unix: accessNowUnix - 7 * 86_400,
		default: false,
	},
	{
		id: "8b0d2f4a6c8e",
		display_name: "Alice",
		accent: "#eab308",
		owner: false,
		home: "desktop",
		seat: {
			kind: "steam",
			state: "unavailable",
			port: 9777,
			detail: "This profile signs in to an account this host can't start yet.",
		},
		last_used_unix: 0,
		default: false,
	},
];

/** A Windows Server host: the owner, a desktop sharer and a seat in each seat state. */
export const profilesWindows: ProfileAdmin[] = [
	...profilesOne,
	{
		id: "0b7d5e2a91c4",
		display_name: "Anna",
		accent: "#ec4899",
		owner: false,
		home: "desktop",
		last_used_unix: 0,
		default: false,
	},
	{
		id: "9a3f1c2b7e40",
		display_name: "Kid",
		accent: "#f97316",
		owner: false,
		home: "desktop",
		seat: { state: "ready", port: 9777, kind: "desktop" },
		last_used_unix: accessNowUnix - 600,
		default: false,
	},
	{
		id: "7e1f3b5d9a2c",
		display_name: "Leon",
		accent: "#22c55e",
		owner: false,
		home: "desktop",
		seat: {
			state: "occupied",
			port: 9777,
			occupant: "Leon's Deck",
			kind: "desktop",
		},
		last_used_unix: accessNowUnix - 60,
		default: false,
	},
	{
		id: "5a7c9e1b3d5f",
		display_name: "Mia",
		accent: "#a855f7",
		owner: false,
		home: "desktop",
		seat: { state: "starting", port: 9777, kind: "desktop" },
		last_used_unix: accessNowUnix - 86_400,
		default: false,
	},
	{
		id: "3d5f7b9a1c3e",
		display_name: "Oma",
		accent: "#14b8a6",
		owner: false,
		home: "desktop",
		seat: { state: "stopped", port: 9777, kind: "desktop" },
		last_used_unix: accessNowUnix - 7 * 86_400,
		default: false,
	},
	{
		id: "8b0d2f4a6c8e",
		display_name: "Alice",
		accent: "#eab308",
		owner: false,
		home: "desktop",
		seat: {
			kind: "desktop",
			state: "unavailable",
			port: 9777,
			detail: "Remote Desktop licensing has run out for this server.",
		},
		last_used_unix: 0,
		default: false,
	},
];

/**
 * A Linux door: every profile has the owner's seat or one of its own. The owner's own seat is
 * stopped until a device connects (nobody is logged in), a desktop sharer rides on it, and two
 * seats of their own are ready and in use.
 */
export const profilesDoor: ProfileAdmin[] = [
	{
		id: "4f1c3a9b0e27",
		display_name: "Enrico",
		accent: "#3b82f6",
		owner: true,
		home: "desktop",
		seat: { state: "stopped", port: 9778, kind: "shared" },
		last_used_unix: accessNowUnix - 3600,
		default: true,
	},
	{
		id: "0b7d5e2a91c4",
		display_name: "Anna",
		accent: "#ec4899",
		owner: false,
		home: "desktop",
		seat: { state: "stopped", port: 9778, kind: "shared" },
		last_used_unix: 0,
		default: false,
	},
	{
		id: "9a3f1c2b7e40",
		display_name: "Kid",
		accent: "#f97316",
		owner: false,
		home: "bigpicture",
		seat: { state: "stopped", port: 9778, kind: "steam" },
		last_used_unix: accessNowUnix - 600,
		default: false,
	},
	{
		id: "7e1f3b5d9a2c",
		display_name: "Leon",
		accent: "#22c55e",
		owner: false,
		home: "desktop",
		seat: {
			state: "occupied",
			port: 9779,
			occupant: "Leon's Deck",
			kind: "desktop",
		},
		last_used_unix: accessNowUnix - 60,
		default: false,
	},
	{
		id: "3d5f7b9a1c3e",
		display_name: "Oma",
		accent: "#14b8a6",
		owner: false,
		home: "desktop",
		seat: { state: "ready", port: 9780, kind: "desktop" },
		last_used_unix: accessNowUnix - 7 * 86_400,
		default: false,
	},
];

/** Seats on, every check passing. */
export const seatingOn: Seating = {
	enabled: true,
	platform: "windows",
	checks: [
		{
			level: "info",
			code: "windows_build",
			message: "Windows build 26100 meets the 22621 minimum",
		},
		{
			level: "info",
			code: "rds_role",
			message: "The Remote Desktop Session Host role is installed",
		},
	],
};

/** A turn-on the checks refused: still off, the failing lines first. */
export const seatingRefused: Seating = {
	enabled: false,
	platform: "windows",
	checks: [
		{
			level: "info",
			code: "windows_build",
			message: "Windows build 26100 meets the 22621 minimum",
		},
		{
			level: "error",
			code: "rds_role",
			message: "Install the Remote Desktop Session Host role, then try again.",
		},
		{
			level: "warning",
			code: "rds_licensing",
			message:
				"Remote Desktop licensing isn't set up. Seats stop after the grace period.",
		},
	],
};

/** A knock from a device that asked to play as a profile. */
export const pendingWithProfile: PendingDevice = {
	id: 5,
	name: "Kid's iPad",
	fingerprint:
		"a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90",
	age_secs: 5,
	source: "lan",
	until_disconnect: false,
	profile: { id: "9a3f1c2b7e40", display_name: "Kid" },
};
