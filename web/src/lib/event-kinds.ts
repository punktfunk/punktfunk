// The host's event vocabulary, in one place: the event stream subscribes to every kind, the
// activity feed labels each row, and the automation form offers them as hook triggers.
//
// The identifier is never translated. It is what the host publishes, what the SSE `?kinds=` filter
// takes, and what ends up written into the config file, so it stays the host's own spelling and
// the friendly name rides alongside it.
import type { EventKind } from "@/api/gen/model/eventKind";
import { m } from "@/paraglide/messages";

/** A kind the host publishes, from the generated `EventKind` union. */
export type HostEventKind = EventKind["kind"];

// Keyed by the generated union: a kind the host adds fails the typecheck until it has a label.
const CONCRETE = {
	"client.connected": () => m.activity_client_connected(),
	"client.disconnected": () => m.activity_client_disconnected(),
	"session.started": () => m.activity_session_started(),
	"session.ended": () => m.activity_session_ended(),
	"stream.started": () => m.activity_stream_started(),
	"stream.stopped": () => m.activity_stream_stopped(),
	"game.launching": () => m.activity_game_launching(),
	"game.running": () => m.activity_game_running(),
	"game.window": () => m.activity_game_window(),
	"game.exited": () => m.activity_game_exited(),
	"pairing.pending": () => m.activity_pairing_pending(),
	"pairing.completed": () => m.activity_pairing_completed(),
	"pairing.denied": () => m.activity_pairing_denied(),
	"access.granted": () => m.activity_access_granted(),
	"access.changed": () => m.activity_access_changed(),
	"access.expired": () => m.activity_access_expired(),
	"display.created": () => m.activity_display_created(),
	"display.released": () => m.activity_display_released(),
	"library.changed": () => m.activity_library_changed(),
	"emulators.changed": () => m.activity_emulators_changed(),
	"downloads.changed": () => m.activity_downloads_changed(),
	"update.available": () => m.activity_update_available(),
	"update.applied": () => m.activity_update_applied(),
	"plugins.changed": () => m.activity_plugins_changed(),
	"store.changed": () => m.activity_store_changed(),
	"settings.changed": () => m.activity_settings_changed(),
	"action.invoked": () => m.activity_action_invoked(),
	"host.started": () => m.activity_host_started(),
	"host.stopping": () => m.activity_host_stopping(),
} satisfies Record<HostEventKind, () => string>;

/** Every kind the host publishes. */
export const HOST_EVENT_KINDS = Object.keys(CONCRETE) as HostEventKind[];

const WILDCARD: Record<string, () => string> = {
	client: () => m.event_any_client(),
	session: () => m.event_any_session(),
	stream: () => m.event_any_stream(),
	game: () => m.event_any_game(),
	pairing: () => m.event_any_pairing(),
	display: () => m.event_any_display(),
};

/** Every kind, each domain with a wildcard led by its `domain.*`: the hook filter's choices. */
export const EVENT_KINDS: string[] = HOST_EVENT_KINDS.flatMap((kind, i) => {
	const domain = kind.split(".")[0] ?? "";
	const first = HOST_EVENT_KINDS[i - 1]?.split(".")[0] !== domain;
	return first && domain in WILDCARD ? [`${domain}.*`, kind] : [kind];
});

/**
 * A kind in words, falling back to the identifier itself.
 *
 * The fallback is the point: a host that starts publishing a kind this console has never heard of
 * still gets a readable row and a selectable hook, rather than a blank one.
 */
export function eventKindLabel(kind: string): string {
	const wildcard = kind.endsWith(".*") && WILDCARD[kind.slice(0, -2)];
	if (wildcard) return wildcard();
	return CONCRETE[kind as HostEventKind]?.() ?? kind;
}
