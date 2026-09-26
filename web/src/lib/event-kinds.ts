// The host's event vocabulary, in one place.
//
// Two surfaces read these: the activity feed labels the kind on every row, and the automation form
// picks one to hook on. They were separate lists — the feed translated `session.started` and the
// form showed the raw identifier — so the same event had a name in one place and not the other.
//
// The identifier is never translated. It is what the host publishes, what the SSE `?kinds=` filter
// takes, and what ends up written into the config file, so it stays the host's own spelling and
// the friendly name rides alongside it.
import { m } from "@/paraglide/messages";

/** The event kinds the host publishes, plus the `domain.*` wildcards the hook filter accepts. */
export const EVENT_KINDS = [
	"client.*",
	"client.connected",
	"client.disconnected",
	"session.*",
	"session.started",
	"session.ended",
	"stream.*",
	"stream.started",
	"stream.stopped",
	"game.*",
	"game.launching",
	"game.running",
	"game.exited",
	"pairing.*",
	"pairing.pending",
	"pairing.completed",
	"pairing.denied",
	"display.*",
	"display.created",
	"display.released",
	"library.changed",
	"update.available",
	"update.applied",
	"host.started",
	"host.stopping",
	"plugins.changed",
	"store.changed",
	"settings.changed",
] as const;

const CONCRETE: Record<string, () => string> = {
	"client.connected": () => m.activity_client_connected(),
	"client.disconnected": () => m.activity_client_disconnected(),
	"session.started": () => m.activity_session_started(),
	"session.ended": () => m.activity_session_ended(),
	"stream.started": () => m.activity_stream_started(),
	"stream.stopped": () => m.activity_stream_stopped(),
	"game.launching": () => m.activity_game_launching(),
	"game.running": () => m.activity_game_running(),
	"game.exited": () => m.activity_game_exited(),
	"pairing.pending": () => m.activity_pairing_pending(),
	"pairing.completed": () => m.activity_pairing_completed(),
	"pairing.denied": () => m.activity_pairing_denied(),
	"display.created": () => m.activity_display_created(),
	"display.released": () => m.activity_display_released(),
	"library.changed": () => m.activity_library_changed(),
	"update.available": () => m.activity_update_available(),
	"update.applied": () => m.activity_update_applied(),
	"plugins.changed": () => m.activity_plugins_changed(),
	"store.changed": () => m.activity_store_changed(),
	"settings.changed": () => m.activity_settings_changed(),
	"host.started": () => m.activity_host_started(),
	"host.stopping": () => m.activity_host_stopping(),
};

const WILDCARD: Record<string, () => string> = {
	client: () => m.event_any_client(),
	session: () => m.event_any_session(),
	stream: () => m.event_any_stream(),
	game: () => m.event_any_game(),
	pairing: () => m.event_any_pairing(),
	display: () => m.event_any_display(),
};

/**
 * A kind in words, falling back to the identifier itself.
 *
 * The fallback is the point: a host that starts publishing a kind this console has never heard of
 * still gets a readable row and a selectable hook, rather than a blank one.
 */
export function eventKindLabel(kind: string): string {
	const wildcard = kind.endsWith(".*") && WILDCARD[kind.slice(0, -2)];
	if (wildcard) return wildcard();
	return CONCRETE[kind]?.() ?? kind;
}
