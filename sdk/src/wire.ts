// The lifecycle-event wire schemas (RFC §4/§7) — hand-written as a discriminated union on
// `kind` for precise types and decode errors. The REST surface is generated (./gen/schemas.ts);
// the event stream's `text/event-stream` payload is not expressible there, and the host's
// Rust-side JSON snapshot tests (crates/punktfunk-host/src/events.rs) are the wire's source of
// truth — this file mirrors them. Additive-only within `schema: 1`: decoding tolerates unknown
// keys (Effect's default), and an unknown `kind` surfaces on the raw channel, never a throw.
import { Schema as S } from "effect";

export const Plane = S.Literals(["native", "gamestream"]);
export type Plane = S.Schema.Type<typeof Plane>;

export const DisconnectReason = S.Literals(["quit", "timeout", "error"]);
export type DisconnectReason = S.Schema.Type<typeof DisconnectReason>;

/** The settings preset a client dialled with. The id is stable across a rename. */
export const PresetRef = S.Struct({
	id: S.String,
	name: S.String,
});
export type PresetRef = S.Schema.Type<typeof PresetRef>;

export const ClientRef = S.Struct({
	name: S.String,
	fingerprint: S.optional(S.String),
	plane: Plane,
	preset: S.optional(PresetRef),
});
export type ClientRef = S.Schema.Type<typeof ClientRef>;

export const SessionRef = S.Struct({
	id: S.Number,
	client: S.String,
	fingerprint: S.optional(S.String),
	mode: S.String,
	hdr: S.Boolean,
	plane: S.optional(Plane),
	preset: S.optional(PresetRef),
});
export type SessionRef = S.Schema.Type<typeof SessionRef>;

export const StreamRef = S.Struct({
	mode: S.String,
	hdr: S.Boolean,
	client: S.String,
	fingerprint: S.optional(S.String),
	app: S.optional(S.String),
	plane: Plane,
	preset: S.optional(PresetRef),
});
export type StreamRef = S.Schema.Type<typeof StreamRef>;

export const DeviceRef = S.Struct({
	name: S.String,
	fingerprint: S.String,
	plane: Plane,
});
export type DeviceRef = S.Schema.Type<typeof DeviceRef>;

/** A launched game, as the `game.*` events identify it. */
export const GameRef = S.Struct({
	/** Store-qualified library id (`steam:570`). Absent for an operator-typed GameStream command. */
	app: S.optional(S.String),
	title: S.String,
	store: S.optional(S.String),
	/** Client-supplied device name of the session that launched it; may be empty. */
	client: S.String,
	/** Stable id of the device that launched it. Absent for an anonymous client. */
	fingerprint: S.optional(S.String),
	plane: Plane,
	/** The preset of the session that launched it. */
	preset: S.optional(PresetRef),
});
export type GameRef = S.Schema.Type<typeof GameRef>;

/** Why a launched game is no longer running. */
export const GameEndReason = S.Literals(["exited", "terminated"]);
export type GameEndReason = S.Schema.Type<typeof GameEndReason>;

/** The `{seq, ts_ms, schema}` envelope every event carries. */
const envelope = {
	seq: S.Number,
	ts_ms: S.Number,
	schema: S.Number,
} as const;

export const ClientConnected = S.Struct({
	...envelope,
	kind: S.Literal("client.connected"),
	client: ClientRef,
});
export const ClientDisconnected = S.Struct({
	...envelope,
	kind: S.Literal("client.disconnected"),
	client: ClientRef,
	reason: DisconnectReason,
});
export const SessionStarted = S.Struct({
	...envelope,
	kind: S.Literal("session.started"),
	session: SessionRef,
});
/** Why a session ended, in the same words the client maps from the QUIC close. */
export const SessionEndReason = S.Literals([
	"local",
	"game_exited",
	"host_ended",
	"host_error",
	"lost",
	"stopped_by_operator",
]);
export type SessionEndReason = S.Schema.Type<typeof SessionEndReason>;

/** Client datagrams the session took, by class. */
export const InputCounts = S.Struct({
	events: S.Number,
	mic: S.Number,
	rich: S.Number,
	/** Offers the input queue refused because it was full — not a wire loss. */
	dropped: S.Number,
});
export type InputCounts = S.Schema.Type<typeof InputCounts>;

/** Gyro arrivals, and the gaps of 500 ms or more among them. */
export const GyroCadence = S.Struct({
	samples: S.Number,
	stalls: S.Number,
});
export type GyroCadence = S.Schema.Type<typeof GyroCadence>;

/** Audio egress for the whole session, not the 30 s window the host log prints. */
export const AudioEgress = S.Struct({
	sent: S.Number,
	infilled: S.Number,
	late: S.Number,
	max_late_ms: S.Number,
	reanchors: S.Number,
});
export type AudioEgress = S.Schema.Type<typeof AudioEgress>;

/** What the encoder's target did. `avg_kbps` is a mean of the targets, not time-weighted. */
export const BitrateSpan = S.Struct({
	min_kbps: S.Number,
	avg_kbps: S.Number,
	max_kbps: S.Number,
	adaptive_steps: S.Number,
});
export type BitrateSpan = S.Schema.Type<typeof BitrateSpan>;

/**
 * Everything the host knows about a finished session. `GET /api/v1/session/last` returns the
 * same shape. A total the host does not keep per session is absent, never zero.
 */
export const SessionSummary = S.Struct({
	id: S.Number,
	client: S.String,
	client_name: S.optional(S.String),
	started_unix: S.Number,
	duration_s: S.Number,
	mode: S.String,
	hdr: S.Boolean,
	join: S.Boolean,
	codec: S.String,
	bit_depth: S.Number,
	chroma: S.String,
	bitrate_kbps: S.Number,
	bitrate: S.optional(BitrateSpan),
	frames_sent: S.optional(S.Number),
	frames_dropped: S.optional(S.Number),
	input: InputCounts,
	gyro: S.optional(GyroCadence),
	audio: S.optional(AudioEgress),
	bringup_ms: S.Number,
	path_mtu: S.optional(S.Number),
	ended: SessionEndReason,
});
export type SessionSummary = S.Schema.Type<typeof SessionSummary>;

export const SessionEnded = S.Struct({
	...envelope,
	kind: S.Literal("session.ended"),
	session: SessionRef,
	summary: SessionSummary,
});
export const StreamStarted = S.Struct({
	...envelope,
	kind: S.Literal("stream.started"),
	stream: StreamRef,
});
export const StreamStopped = S.Struct({
	...envelope,
	kind: S.Literal("stream.stopped"),
	stream: StreamRef,
});
/**
 * A launched game is about to start. The host waits for plugins and automations that hold this
 * stage, each up to its own deadline. It fires only when the host spawns the game, not on adopt.
 */
export const GameLaunching = S.Struct({
	...envelope,
	kind: S.Literal("game.launching"),
	game: GameRef,
});
/**
 * A launched game's process was seen running — not merely its launcher spawned. Fires once per
 * session that launched a title.
 */
export const GameRunning = S.Struct({
	...envelope,
	kind: S.Literal("game.running"),
	game: GameRef,
});
/**
 * A launched game is gone. `reason` separates the player quitting (`exited` — which is what ends the
 * streaming session, when that is enabled) from the host ending it per the lifetime policy
 * (`terminated`).
 */
export const GameExited = S.Struct({
	...envelope,
	kind: S.Literal("game.exited"),
	game: GameRef,
	reason: GameEndReason,
});
/** The game's own window reached the screen, often well after its process. */
export const GameWindow = S.Struct({
	...envelope,
	kind: S.Literal("game.window"),
	game: GameRef,
	/** Title the compositor or the desktop reports for that window. */
	title: S.String,
	/** Wayland `app_id` or X11 class; empty on Windows. */
	app_id: S.String,
});
export const PairingPending = S.Struct({
	...envelope,
	kind: S.Literal("pairing.pending"),
	device: DeviceRef,
});
export const PairingCompleted = S.Struct({
	...envelope,
	kind: S.Literal("pairing.completed"),
	device: DeviceRef,
});
export const PairingDenied = S.Struct({
	...envelope,
	kind: S.Literal("pairing.denied"),
	device: DeviceRef,
});
/** The operator chose a device's access at pairing. `grants` holds `GRANT_*` bits. */
export const AccessGranted = S.Struct({
	...envelope,
	kind: S.Literal("access.granted"),
	device: DeviceRef,
	grants: S.Number,
	/** Unix seconds; absent means permanent. */
	expires_unix: S.optional(S.Number),
});
export const AccessChanged = S.Struct({
	...envelope,
	kind: S.Literal("access.changed"),
	device: DeviceRef,
	grants: S.Number,
	expires_unix: S.optional(S.Number),
});
export const AccessExpired = S.Struct({
	...envelope,
	kind: S.Literal("access.expired"),
	device: DeviceRef,
});
export const DisplayCreated = S.Struct({
	...envelope,
	kind: S.Literal("display.created"),
	backend: S.String,
	mode: S.String,
});
export const DisplayReleased = S.Struct({
	...envelope,
	kind: S.Literal("display.released"),
	count: S.Number,
});
export const LibraryChanged = S.Struct({
	...envelope,
	kind: S.Literal("library.changed"),
	source: S.String,
});
export const UpdateAvailable = S.Struct({
	...envelope,
	kind: S.Literal("update.available"),
	version: S.String,
	channel: S.String,
	install_kind: S.String,
});
export const UpdateApplied = S.Struct({
	...envelope,
	kind: S.Literal("update.applied"),
	from: S.String,
	to: S.String,
});
/** A plugin registered, restarted, deregistered or expired. Re-read `GET /api/v1/plugins`. */
export const PluginsChanged = S.Struct({
	...envelope,
	kind: S.Literal("plugins.changed"),
	id: S.String,
});
export const StoreChanged = S.Struct({
	...envelope,
	kind: S.Literal("store.changed"),
});
export const SettingsChanged = S.Struct({
	...envelope,
	kind: S.Literal("settings.changed"),
	ids: S.Array(S.String),
});
/** A console or device action was accepted, or later failed (`failed: <cause>`). */
export const ActionInvoked = S.Struct({
	...envelope,
	kind: S.Literal("action.invoked"),
	id: S.String,
	device: S.optional(DeviceRef),
	outcome: S.String,
});
export const HostStarted = S.Struct({
	...envelope,
	kind: S.Literal("host.started"),
	version: S.String,
	gamestream: S.Boolean,
});
export const HostStopping = S.Struct({
	...envelope,
	kind: S.Literal("host.stopping"),
});

/** Every known lifecycle event — discriminated on `kind`. */
export const HostEvent = S.Union([
	ClientConnected,
	ClientDisconnected,
	SessionStarted,
	SessionEnded,
	StreamStarted,
	StreamStopped,
	GameLaunching,
	GameRunning,
	GameWindow,
	GameExited,
	PairingPending,
	PairingCompleted,
	PairingDenied,
	AccessGranted,
	AccessChanged,
	AccessExpired,
	DisplayCreated,
	DisplayReleased,
	LibraryChanged,
	UpdateAvailable,
	UpdateApplied,
	PluginsChanged,
	StoreChanged,
	SettingsChanged,
	ActionInvoked,
	HostStarted,
	HostStopping,
]);
export type HostEvent = S.Schema.Type<typeof HostEvent>;

/** The known event kinds (for filters and the facade's `on()`). */
export type HostEventKind = HostEvent["kind"];

/** Narrow a HostEvent by kind: `EventOf<"stream.started">`. */
export type EventOf<K extends HostEventKind> = Extract<HostEvent, { kind: K }>;

/**
 * Decode one event JSON into a [`HostEvent`], as a [`Result`]: `Success` for a known kind,
 * `Failure` for an unknown/undecodable one (a newer host — rides the raw channel, never throws).
 * (v4 replaced `Either` with `Result`; the callers branch on `Result.isFailure`.)
 */
export const decodeHostEvent = S.decodeUnknownResult(HostEvent);

/**
 * Does `pattern` select `kind`? Exact kinds (`stream.started`) or `domain.*` prefixes on the
 * dot boundary — the same vocabulary as the host's SSE `?kinds=` filter and hooks `on:` field.
 */
export const kindMatches = (pattern: string, kind: string): boolean =>
	pattern.endsWith(".*")
		? kind.startsWith(pattern.slice(0, -1)) // "stream.*" → prefix "stream."
		: pattern === kind;
