import * as Data from "effect/Data"
import * as Effect from "effect/Effect"
import type { SchemaError } from "effect/Schema"
import * as Schema from "effect/Schema"
import * as Stream from "effect/Stream"
import * as Sse from "effect/unstable/encoding/Sse"
import * as HttpClient from "effect/unstable/http/HttpClient"
import * as HttpClientError from "effect/unstable/http/HttpClientError"
import * as HttpClientRequest from "effect/unstable/http/HttpClientRequest"
import * as HttpClientResponse from "effect/unstable/http/HttpClientResponse"
// non-recursive definitions
export type AccessPathOutcome = { readonly "outcome": string, readonly "path": string }
export const AccessPathOutcome = Schema.Struct({ "outcome": Schema.String, "path": Schema.String }).annotate({ "description": "One path's answer: `granted`, `pending`, `denied`, or `refused:<rule>`." })
export type AccessPathRequest = { readonly "path": string, readonly "write"?: boolean }
export const AccessPathRequest = Schema.Struct({ "path": Schema.String.annotate({ "description": "The directory the plugin wants, absolute on the host." }), "write": Schema.optionalKey(Schema.Boolean.annotate({ "description": "`true` asks for write access too; a grant is read-only unless the operator allows it." })) })
export type ActionInfo = { readonly "available": boolean, readonly "danger": boolean, readonly "group": string, readonly "id": string, readonly "permitted": boolean, readonly "title": string, readonly "unavailable_reason"?: string | null }
export const ActionInfo = Schema.Struct({ "available": Schema.Boolean.annotate({ "description": "Platform probe. A VM that cannot S3 lists sleep as unavailable rather than a dead switch." }), "danger": Schema.Boolean.annotate({ "description": "Double-confirm hint: reboot/shutdown lose state." }), "group": Schema.String, "id": Schema.String.annotate({ "description": "Invoke path parameter (`power.sleep`, …)." }), "permitted": Schema.Boolean.annotate({ "description": "Whether THIS caller may invoke it. Admin: always. Cert: the live `GRANT_POWER` bit for\npower, its own live session for `display.next`." }), "title": Schema.String.annotate({ "description": "Clients localize known ids and fall back to this for unknown ones." }), "unavailable_reason": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])) }).annotate({ "description": "One action as the caller sees it (`GET /actions`)." })
export type ActionOutcome = { readonly "monitor": string }
export const ActionOutcome = Schema.Struct({ "monitor": Schema.String.annotate({ "description": "Connector streamed from now on." }) }).annotate({ "description": "What `display.next` did (`200`). Power actions answer `202` with no body." })
export type ActiveGame = { readonly "app_id"?: string | null, readonly "awaiting_window"?: boolean, readonly "client": string, readonly "grace_remaining_s"?: never, readonly "plane": "native" | "gamestream", readonly "session_id"?: number, readonly "state": string, readonly "store"?: string | null, readonly "title": string }
export const ActiveGame = Schema.Struct({ "app_id": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Store-qualified library id (`steam:570`); matches `GET /library`. Absent for a typed GameStream command." })), "awaiting_window": Schema.optionalKey(Schema.Boolean.annotate({ "description": "Present and true while `running` on a host that will report `window` next. A launch hold\nwaits for that instead of revealing a game that is still loading." })), "client": Schema.String.annotate({ "description": "Client-supplied device name of the session that launched it; may be empty." }), "grace_remaining_s": Schema.optionalKey(Schema.Never), "plane": Schema.Literals(["native", "gamestream"]).annotate({ "description": "`native` or `gamestream`." }), "session_id": Schema.optionalKey(Schema.Number.annotate({ "description": "Streaming session; `null` while waiting out the reconnect window. Pass it to\n`DELETE /session/{id}` to stop that one session.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))), "state": Schema.String.annotate({ "description": "`launching` | `running` | `window` (its window is on the streamed screen) | `exited` |\n`untracked` (exit will never be seen) | `grace` (reconnect window)." }), "store": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Which store surfaced it (`steam`, `heroic`, `custom`, …), when known." })), "title": Schema.String })
export type ApiActiveGpu = { readonly "backend": string, readonly "id": string, readonly "name": string, readonly "sessions": number, readonly "vendor": string }
export const ApiActiveGpu = Schema.Struct({ "backend": Schema.String.annotate({ "description": "`nvenc` | `amf` | `qsv` | `mf` | `vaapi` | `software`." }), "id": Schema.String.annotate({ "description": "Matches a `gpus` entry; empty when encoding on CPU/software." }), "name": Schema.String, "sessions": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "vendor": Schema.String.annotate({ "description": "`nvidia` | `amd` | `intel` | `other`." }) }).annotate({ "description": "GPU live encode sessions are on now (not the next-session pick)." })
export type ApiCodec = "h264" | "hevc" | "av1" | "pyrowave"
export const ApiCodec = Schema.Literals(["h264", "hevc", "av1", "pyrowave"]).annotate({ "description": "Wire token is the stack's canonical codec name (`Codec::label`). `H265` serializes as `\"hevc\"`, not `\"h265\"`." })
export type ApiDisplayInfo = { readonly "backend": string, readonly "client"?: string | null, readonly "display_index": number, readonly "expires_in_ms"?: never, readonly "group": number, readonly "identity_slot"?: never, readonly "mode": string, readonly "sessions": number, readonly "slot": number, readonly "state": string, readonly "topology": string, readonly "x": number, readonly "y": number }
export const ApiDisplayInfo = Schema.Struct({ "backend": Schema.String.annotate({ "description": "`pf-vdisplay`, `kwin`, …" }), "client": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "display_index": Schema.Number.annotate({ "description": "Ordinal within the group, acquire order, 0-based.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "expires_in_ms": Schema.optionalKey(Schema.Never), "group": Schema.Number.annotate({ "description": "Shared-desktop group id; same group = one desktop.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "identity_slot": Schema.optionalKey(Schema.Never), "mode": Schema.String.annotate({ "description": "`WIDTHxHEIGHT@HZ`." }), "sessions": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "slot": Schema.Number.annotate({ "description": "Stable-enough id for the `/display/release` `slot` argument.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "state": Schema.String.annotate({ "description": "`active` | `lingering` | `pinned`." }), "topology": Schema.String.annotate({ "description": "Group topology: `extend` | `primary` | `exclusive`." }), "x": Schema.Number.annotate({ "description": "Desktop-space top-left (auto-row or manual layout).", "format": "int32" }).check(Schema.isInt()), "y": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()) }).annotate({ "description": "One live or kept virtual display." })
export type ApiError = { readonly "error": string }
export const ApiError = Schema.Struct({ "error": Schema.String }).annotate({ "description": "Envelope for every non-2xx body." })
export type ApiGpu = { readonly "id": string, readonly "name": string, readonly "vendor": string, readonly "vram_mb": number }
export const ApiGpu = Schema.Struct({ "id": Schema.String.annotate({ "description": "`vendorid-deviceid-occurrence` (hex PCI). Stable across reboot/driver; not an index or LUID." }), "name": Schema.String, "vendor": Schema.String.annotate({ "description": "`nvidia` | `amd` | `intel` | `other`." }), "vram_mb": Schema.Number.annotate({ "description": "0 when the platform does not expose dedicated VRAM.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "Hardware GPU. Software/WARP adapters are never listed." })
export type ApiMonitorInfo = { readonly "connector": string, readonly "description": string, readonly "enabled": boolean, readonly "managed": boolean, readonly "mode": string, readonly "primary": boolean, readonly "scale": number, readonly "selected": boolean, readonly "x": number, readonly "y": number }
export const ApiMonitorInfo = Schema.Struct({ "connector": Schema.String.annotate({ "description": "Connector (`DP-1`, `HDMI-A-2`) — the value `PUNKTFUNK_CAPTURE_MONITOR` takes." }), "description": Schema.String.annotate({ "description": "Picker label (`make model`, else the connector)." }), "enabled": Schema.Boolean.annotate({ "description": "Driven right now. Disabled heads stay listed so they are not missing from the picker." }), "managed": Schema.Boolean.annotate({ "description": "Best-effort: one of our virtual displays, not a real head. Reliable on KWin only." }), "mode": Schema.String.annotate({ "description": "`WIDTHxHEIGHT@HZ` of the current mode (size only when the refresh is unknown)." }), "primary": Schema.Boolean, "scale": Schema.Number.annotate({ "format": "double" }).check(Schema.isFinite()), "selected": Schema.Boolean.annotate({ "description": "True when `PUNKTFUNK_CAPTURE_MONITOR` currently names this monitor." }), "x": Schema.Number.annotate({ "description": "Desktop-space top-left. Distinguishes two heads of the same size.", "format": "int32" }).check(Schema.isInt()), "y": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()) }).annotate({ "description": "Physical monitor as the compositor reports it." })
export type ApiSelectedGpu = { readonly "id": string, readonly "name": string, readonly "source": string, readonly "vendor": string }
export const ApiSelectedGpu = Schema.Struct({ "id": Schema.String, "name": Schema.String, "source": Schema.String.annotate({ "description": "`preference` | `env` | `auto` | `preference_missing` (manual pick absent → auto, still stream)." }), "vendor": Schema.String.annotate({ "description": "`nvidia` | `amd` | `intel` | `other`." }) }).annotate({ "description": "GPU the next session opens on. A running session keeps the GPU it already opened." })
export type ApplyRequest = { readonly "force"?: boolean }
export const ApplyRequest = Schema.Struct({ "force": Schema.optionalKey(Schema.Boolean.annotate({ "description": "Apply even with a live stream (the stream drops when the host restarts)." })) })
export type ApprovePending = { readonly "expires_in_secs"?: never, readonly "grants"?: never, readonly "name"?: string | null, readonly "until_disconnect"?: boolean | null }
export const ApprovePending = Schema.Struct({ "expires_in_secs": Schema.optionalKey(Schema.Never), "grants": Schema.optionalKey(Schema.Never), "name": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Label; defaults to the name the device knocked with." })), "until_disconnect": Schema.optionalKey(Schema.Union([Schema.Boolean, Schema.Null]).annotate({ "description": "Drop the device's record once its last session ends, rather than at a clock time.\nCombines with `expires_in_secs`: whichever comes first ends the grant. Send it with an\naccess level, never alone (400) — like the other access fields it is part of a whole\nreplacement, so sending `grants` without it clears it." })) }).annotate({ "description": "Approve body. `{}` keeps the knock name and, on re-approve, stored access\n(full/permanent on a first pairing)." })
export type ArmNativePairing = { readonly "expires_in_secs"?: never, readonly "fingerprint"?: string | null, readonly "grants"?: never, readonly "ttl_secs"?: never, readonly "until_disconnect"?: boolean | null }
export const ArmNativePairing = Schema.Struct({ "expires_in_secs": Schema.optionalKey(Schema.Never), "fingerprint": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Hex SHA-256 fingerprint that may consume this window. Omit for any\ndevice (trusted-LAN only)." })), "grants": Schema.optionalKey(Schema.Never), "ttl_secs": Schema.optionalKey(Schema.Never), "until_disconnect": Schema.optionalKey(Schema.Union([Schema.Boolean, Schema.Null]).annotate({ "description": "Drop the device's record once its last session ends, rather than at a clock time.\nCombines with `expires_in_secs`: whichever comes first ends the grant. Send it with an\naccess level, never alone (400) — like the other access fields it is part of a whole\nreplacement, so sending `grants` without it clears it." })) })
export type ArtPickInput = { readonly "kind": string, readonly "url"?: string | null }
export const ArtPickInput = Schema.Struct({ "kind": Schema.String.annotate({ "description": "`portrait`, `hero`, `logo` or `header`." }), "url": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "An `http(s)` URL; `null` clears the pick." })) }).annotate({ "description": "`PUT /library/picks/{id}`." })
export type Artwork = { readonly "header"?: string | null, readonly "hero"?: string | null, readonly "logo"?: string | null, readonly "portrait"?: string | null }
export const Artwork = Schema.Struct({ "header": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Steam `header.jpg` — the universal fallback." })), "hero": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Steam `library_hero.jpg`." })), "logo": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Steam `logo.png`." })), "portrait": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Steam `library_600x900.jpg`." })) }).annotate({ "description": "Cover art. The client prefers `portrait` for a grid and falls back to `header`\nwhen a title has no 600×900 capsule (common for older Steam apps)." })
export type AudioSessions = "all" | "owner" | "joined" | "launcher"
export const AudioSessions = Schema.Literals(["all", "owner", "joined", "launcher"]).annotate({ "description": "Which sessions hear a title's audio (`audio.sessions` on a custom entry)." })
export type AvailableCompositor = { readonly "available": boolean, readonly "default": boolean, readonly "id": string, readonly "label": string }
export const AvailableCompositor = Schema.Struct({ "available": Schema.Boolean.annotate({ "description": "Usable now: the live session's compositor, or gamescope if its binary is installed." }), "default": Schema.Boolean.annotate({ "description": "True for the backend an `Auto` (unspecified) request resolves to right now." }), "id": Schema.String.annotate({ "description": "Stable id (`kwin` | `wlroots` | `mutter` | `gamescope`); pass to `--compositor`." }), "label": Schema.String }).annotate({ "description": "A compositor backend and whether it is usable now." })
export type CaptureMeta = { readonly "client": string, readonly "codec": string, readonly "duration_ms": number, readonly "encoder_backend"?: string, readonly "fps": number, readonly "gpu"?: string, readonly "height": number, readonly "id": string, readonly "kind": string, readonly "sample_count": number, readonly "started_unix_ms": number, readonly "truncated"?: boolean, readonly "width": number }
export const CaptureMeta = Schema.Struct({ "client": Schema.String.annotate({ "description": "Fingerprint prefix, or `\"\"` if unknown." }), "codec": Schema.String.annotate({ "description": "`\"h264\" | \"hevc\" | \"av1\"`." }), "duration_ms": Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "encoder_backend": Schema.optionalKey(Schema.String.annotate({ "description": "Backend that actually opened (`\"nvenc\"`, `\"vaapi\"`, `\"vulkan\"`, `\"amf\"`,\n`\"qsv\"`, `\"software\"`, …), from `pf_gpu::active()`. Stage timings are\nunreadable without it. `\"\"` if nothing was streaming at registration." })), "fps": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "gpu": Schema.optionalKey(Schema.String.annotate({ "description": "GPU name from `pf_gpu::active()`, or `\"\"`." })), "height": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "id": Schema.String.annotate({ "description": "Filename stem, e.g. `2026-06-26T20-14-03Z_5120x1440`." }), "kind": Schema.String.annotate({ "description": "`\"native\" | \"gamestream\"`." }), "sample_count": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "started_unix_ms": Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "truncated": Schema.optionalKey(Schema.Boolean.annotate({ "description": "The sample cap was hit: samples past it were dropped and the start kept." })), "width": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "Filename stem plus negotiated mode/codec/client. On-disk head;\n[`StatsRecorder::list`] returns this without the sample body." })
export type CaptureStage = { readonly "outcome": string, readonly "stage": string, readonly "took_ms": number }
export const CaptureStage = Schema.Struct({ "outcome": Schema.String.annotate({ "description": "`applied` / `failed` / `unsupported` / `timed_out`." }), "stage": Schema.String.annotate({ "description": "`encoder_reset` / `swap_chain_reset` / `presentation_reset` / `driver_cycle`." }), "took_ms": Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "One recovery rung of an episode." })
export type CatalogEntry = { readonly "author": string, readonly "blocked"?: string | null, readonly "categories": ReadonlyArray<string>, readonly "compatible": boolean, readonly "description": string, readonly "detected"?: boolean | null, readonly "homepage"?: string | null, readonly "icon"?: string | null, readonly "id": string, readonly "incompatible_reason"?: string | null, readonly "installed_version"?: string | null, readonly "license"?: string | null, readonly "min_host"?: string | null, readonly "pkg": string, readonly "platforms": ReadonlyArray<string>, readonly "reviewed_at"?: string | null, readonly "source": string, readonly "tier": string, readonly "title": string, readonly "update_available": boolean, readonly "version": string }
export const CatalogEntry = Schema.Struct({ "author": Schema.String, "blocked": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Revocation of the catalogued version; listed, never offered quietly." })), "categories": Schema.Array(Schema.String).annotate({ "description": "Browse filters; the Game-sources add-rail is exactly the `library` entries." }), "compatible": Schema.Boolean, "description": Schema.String, "detected": Schema.optionalKey(Schema.Union([Schema.Boolean, Schema.Null]).annotate({ "description": "Host-side existence probe for the launcher this plugin scans. `null` = no probe for this\nplatform (unknown, not \"not installed\")." })), "homepage": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "icon": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "id": Schema.String, "incompatible_reason": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "installed_version": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "license": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "min_host": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "pkg": Schema.String, "platforms": Schema.Array(Schema.String), "reviewed_at": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "When unom reviewed this exact tarball (built-in source only)." })), "source": Schema.String, "tier": Schema.String.annotate({ "description": "`verified` (built-in) or `external` (operator-added). Never `unverified`: those come from a\nraw spec and are not listed." }), "title": Schema.String, "update_available": Schema.Boolean, "version": Schema.String.annotate({ "description": "The one installable version this entry pins." }) })
export type Challenge = { readonly "expires_in": number, readonly "nonce": string }
export const Challenge = Schema.Struct({ "expires_in": Schema.Number.annotate({ "description": "Seconds this nonce remains signable.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "nonce": Schema.String.annotate({ "description": "32 random bytes, lowercase hex. Single-use, and short-lived." }) }).annotate({ "description": "`POST /auth/device/challenge` → the nonce to sign." })
export type CheckSource = "startup" | "event" | "refresh"
export const CheckSource = Schema.Literals(["startup", "event", "refresh"]).annotate({ "description": "Origin of a verdict. `Event` is for live feeds (push on transition); v1 emits `Startup` and\n`Refresh` only." })
export type CheckStatus = "ok" | "warn" | "fail" | "inapplicable"
export const CheckStatus = Schema.Literals(["ok", "warn", "fail", "inapplicable"]).annotate({ "description": "Probe result. `Inapplicable` is not `Ok`: \"never on this box\" and \"works here\" are different\nanswers on the troubleshooting page." })
export type ClientLogMeta = { readonly "device_name": string, readonly "fingerprint_prefix": string, readonly "id": string, readonly "received_ms": number, readonly "size_bytes": number }
export const ClientLogMeta = Schema.Struct({ "device_name": Schema.String.annotate({ "description": "Paired device name at upload, filesystem-sanitized." }), "fingerprint_prefix": Schema.String.annotate({ "description": "First 16 hex chars of the pairing fingerprint — enough to match the roster\nwithout repeating the full identity in every filename." }), "id": Schema.String.annotate({ "description": "Filename stem; pass to fetch/delete." }), "received_ms": Schema.Number.annotate({ "description": "Upload time (unix ms from the file mtime, not the stem timestamp).", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "size_bytes": Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) })
export type ClientLogUploaded = { readonly "id": string }
export const ClientLogUploaded = Schema.Struct({ "id": Schema.String })
export type DecisionInput = "allow" | "deny" | "forget"
export const DecisionInput = Schema.Literals(["allow", "deny", "forget"])
export type DisconnectReason = "quit" | "timeout" | "error"
export const DisconnectReason = Schema.Literals(["quit", "timeout", "error"]).annotate({ "description": "`Quit` is the typed close; `Timeout` is transport idle; `Error` is everything else." })
export type EndGameRequest = { readonly "app_id"?: string | null, readonly "streaming"?: boolean }
export const EndGameRequest = Schema.Struct({ "app_id": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Store-qualified id (`steam:570`); omit to end every waiting game." })), "streaming": Schema.optionalKey(Schema.Boolean.annotate({ "description": "Also end `app_id` where it is on a live session, not only where it is\nwaiting out a reconnect window. Ignored without `app_id`." })) })
export type EndGameResult = { readonly "ended": number }
export const EndGameResult = Schema.Struct({ "ended": Schema.Number.check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) })
export type EnvMarker = { readonly "key": string, readonly "value"?: string | null }
export const EnvMarker = Schema.Struct({ "key": Schema.String, "value": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "`None` matches key presence only — safe solely for one-game-at-a-time launchers." })) }).annotate({ "description": "Plugin-owned stores must name this on [`DetectHint`]; the host does not\nread the launcher's files. Heroic's `HEROIC_APP_NAME` is the only signal\nunder Proton." })
export type GameEndReason = "exited" | "terminated"
export const GameEndReason = Schema.Literals(["exited", "terminated"])
export type GameMeta = { readonly "description"?: string | null, readonly "developer"?: string | null, readonly "genres"?: ReadonlyArray<string>, readonly "platform"?: string | null, readonly "players"?: never, readonly "publisher"?: string | null, readonly "region"?: string | null, readonly "release_year"?: never, readonly "tags"?: ReadonlyArray<string> }
export const GameMeta = Schema.Struct({ "description": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "developer": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "genres": Schema.optionalKey(Schema.Array(Schema.String)), "platform": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "`\"PS2\"`, `\"Xbox 360\"`, `\"SNES\"`, … Installed-store scanners stamp `\"PC\"`;\n`GET /library?platform=` filters on it (case-insensitive)." })), "players": Schema.optionalKey(Schema.Never), "publisher": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "region": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "release_year": Schema.optionalKey(Schema.Never), "tags": Schema.optionalKey(Schema.Array(Schema.String)) }).annotate({ "description": "Optional display metadata, `#[serde(flatten)]`-ed into [`GameEntry`].\n\nValues are free-form strings, not enums — emulation sources (RomM, EmuDeck,\nPlaynite) each have their own vocabulary and the host does not normalize it." })
export type GameOnNewLaunch = "keep" | "end"
export const GameOnNewLaunch = Schema.Literals(["keep", "end"]).annotate({ "description": "What to do with a title this client already has running when it launches a\n**different** one.\n\nA third axis, not a fourth [`GameOnSessionEnd`] value: that one is \"this\nsession is over\"; this one is \"the player asked for something else\". An\noperator who wants a game to survive a disconnect can still want it closed\nwhen they pick another title.\n\nScoped to this client's own launches, and only launches this host performed\n([`crate::launchreg`]). A game the player started at the machine was never\nrecorded, so this cannot close it." })
export type GameOnSessionEnd = "keep" | "on_quit" | "always"
export const GameOnSessionEnd = Schema.Literals(["keep", "on_quit", "always"]).annotate({ "description": "What to do with the launched game when its session ends." })
export type GameRole = "game" | "launcher"
export const GameRole = Schema.Literals(["game", "launcher"]).annotate({ "description": "Presentation hint: ordinary title vs the launcher itself (Steam Big Picture,\nHeroic, Playnite fullscreen). A launcher entry launches, leases, and lists\nlike a game (design D4). Serde-default `game`; skipped when default so the\nwire is unchanged for entries that don't opt in." })
export type GameSession = "auto" | "dedicated"
export const GameSession = Schema.Literals(["auto", "dedicated"]).annotate({ "description": "How a session that launches a game is served\n(`design/gamemode-and-dedicated-sessions.md`). Top-level\n[`DisplayPolicy`] field, not part of [`EffectivePolicy`], so a preset\nnever clobbers it. Linux-only in effect." })
export type Grant = { readonly "at": string, readonly "by": string, readonly "path": string, readonly "write": boolean }
export const Grant = Schema.Struct({ "at": Schema.String, "by": Schema.String, "path": Schema.String, "write": Schema.Boolean }).annotate({ "description": "One root the operator granted, as recorded on disk. `at`/`by` are audit fields:\nRFC3339 and `console`/`cli`/`legacy` (a v1 entry has neither)." })
export type Health = { readonly "abi_version": number, readonly "status": string, readonly "version": string }
export const Health = Schema.Struct({ "abi_version": Schema.Number.annotate({ "description": "`punktfunk-core` C ABI version.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "status": Schema.String.annotate({ "description": "Always `\"ok\"` when the host responds." }), "version": Schema.String.annotate({ "description": "`punktfunk-host` crate version." }) })
export type HiddenState = { readonly "hidden": boolean, readonly "id": string }
export const HiddenState = Schema.Struct({ "hidden": Schema.Boolean, "id": Schema.String })
export type HiddenToggle = { readonly "hidden": boolean }
export const HiddenToggle = Schema.Struct({ "hidden": Schema.Boolean })
export type HostFacts = { readonly "platform": string, readonly "version": string }
export const HostFacts = Schema.Struct({ "platform": Schema.String.annotate({ "description": "`linux` / `windows` / `macos`." }), "version": Schema.String }).annotate({ "description": "The console greys out rows this host cannot install." })
export type HostSettingsPatch = { readonly [x: string]: Schema.Json }
export const HostSettingsPatch = Schema.Record(Schema.String, Schema.Json).annotate({ "description": "Setting id → new value; `null` returns a setting to its default." }).check(Schema.isPropertyNames(Schema.String))
export type HostTheme = { readonly "accent"?: string | null, readonly "mode"?: string | null, readonly "source"?: string | null }
export const HostTheme = Schema.Struct({ "accent": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "`#rrggbb`, or `null` when the desktop has no accent to report." })), "mode": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "`light` | `dark`, or `null` when the desktop expresses no preference." })), "source": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Where the answer came from, for the console's \"Follow host — Windows\" line.\n`null` when nothing answered." })) }).annotate({ "description": "The desktop's appearance, as far as this host can see it." })
export type Identity = "shared" | "per-client" | "per-client-mode"
export const Identity = Schema.Literals(["shared", "per-client", "per-client-mode"]).annotate({ "description": "Stable identity so DEs persist per-display config (KDE scaling). Carried\nas Windows EDID serial + IddCx connector index, KWin per-slot output\nname, and the host-persisted Mutter scale map." })
export type InputCounts = { readonly "dropped": number, readonly "events": number, readonly "mic": number, readonly "rich": number }
export const InputCounts = Schema.Struct({ "dropped": Schema.Number.annotate({ "description": "Offers the input queue refused because it was full. Not a wire loss.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "events": Schema.Number.annotate({ "description": "Keyboard, pointer and touch events.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "mic": Schema.Number.annotate({ "description": "Opus microphone frames.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "rich": Schema.Number.annotate({ "description": "Gamepad and stylus batches.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "Client datagrams this session took, by class.\n\nCounted up to the moment the session was summarized: the reader task ends with the\nconnection, which closes after this, so a last straggler can fall outside." })
export type InstallRequest = { readonly "accept_unverified"?: boolean, readonly "id"?: string | null, readonly "source"?: string | null, readonly "spec"?: string | null }
export const InstallRequest = Schema.Struct({ "accept_unverified": Schema.optionalKey(Schema.Boolean.annotate({ "description": "Required with [`Self::spec`]. Without it the API refuses; a caller cannot skip the\nunverified decision." })), "id": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "With [`Self::source`]: catalogued install." })), "source": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "With [`Self::id`]: catalogued install." })), "spec": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Raw spec (`@scope/name`, `@scope/name@1.2.3`, https tarball or git+https). Nothing reviewed\nit and nothing pins it." })) }).annotate({ "description": "Catalog `{source, id}` or a raw `spec` the operator owns." })
export type InstalledView = { readonly "blocked"?: string | null, readonly "entry_id"?: string | null, readonly "installed_at"?: string | null, readonly "pkg": string, readonly "plugin_id"?: string | null, readonly "running": boolean, readonly "source"?: string | null, readonly "tier": string, readonly "title"?: string | null, readonly "update_available"?: string | null, readonly "version"?: string | null }
export const InstalledView = Schema.Struct({ "blocked": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Revocation of the installed version. Reported, never auto-removed." })), "entry_id": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "installed_at": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "pkg": Schema.String, "plugin_id": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Key for `GET /plugins`." })), "running": Schema.Boolean, "source": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "tier": Schema.String.annotate({ "description": "Install-time tier; unverified stays unverified after the dialog is gone." }), "title": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "update_available": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Catalog version when newer than installed (a version string, not a bool)." })), "version": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])) })
export type JobRef = { readonly "job": string }
export const JobRef = Schema.Struct({ "job": Schema.String })
export type KeepAlive = { readonly "mode": "off" } | { readonly "mode": "duration", readonly "seconds": number } | { readonly "mode": "forever" }
export const KeepAlive = Schema.Union([Schema.Struct({ "mode": Schema.Literal("off") }), Schema.Struct({ "mode": Schema.Literal("duration"), "seconds": Schema.Number.annotate({ "description": "Linger seconds, clamped to `0..=86400` on write. Longer is\n`forever` in practice; unclamped `u32` is ~136 years and a\nnonsense `expires_in_ms`.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "Linger `seconds` after the last session leaves; a reconnect inside\nthe window reuses the display." }), Schema.Struct({ "mode": Schema.Literal("forever") }).annotate({ "description": "Until host shutdown or `POST /display/release` (force-releases\n`Pinned` like `Lingering`). The `gaming-rig` preset selects this." })], { mode: "oneOf" }).annotate({ "description": "Linger after the last client detaches. Tagged on `mode` so the web form\nand OpenAPI stay `{\"mode\":\"off\"}` / `{\"mode\":\"duration\",\"seconds\":N}` /\n`{\"mode\":\"forever\"}`. On gamescope's bare spawn this also keeps the\nnested session and its game." })
export type LaunchSpec = { readonly "args"?: ReadonlyArray<{ readonly "name": string, readonly "value": string }>, readonly "kind": string, readonly "value": string }
export const LaunchSpec = Schema.Struct({ "args": Schema.optionalKey(Schema.Union([Schema.Array(Schema.Struct({ "name": Schema.String.annotate({ "description": "The parameter's name in the template." }), "value": Schema.String }).annotate({ "description": "One `{param}` value for an `exec` template." })).annotate({ "description": "Values for an `exec` template's parameters. Each is checked against the character class\nthe manifest declares for it, and a path must sit inside the paths that manifest names.\nA list rather than a map: a map of strings generates as an untyped object in the SDK." })])), "kind": Schema.String.annotate({ "description": "`\"steam_appid\"` or `\"command\"`." }), "value": Schema.String.annotate({ "description": "Appid for `steam_appid`, the shell command for `command`, or — for `exec` — the name of a\ntemplate in the publishing plugin's manifest." }) }).annotate({ "description": "How the host launches a title. Open-ended so new stores slot in:\n`steam_appid` → `steam steam://rungameid/<value>`; `command` → run `<value>`\nnested in a gamescope session." })
export type LayoutMode = "auto-row" | "manual"
export const LayoutMode = Schema.Literals(["auto-row", "manual"]).annotate({ "description": "Arrangement in desktop space. Computed only in `layout::arrange`:\n`/display/state` and (Linux, KWin only) position apply both consume it." })
export type LinkMinute = { readonly "abr_backoffs": number, readonly "abr_max_kbps": number, readonly "abr_min_kbps": number, readonly "anchor_p": number, readonly "egress_kbps": number, readonly "fec_max_pct": number, readonly "fec_min_pct": number, readonly "gaps": number, readonly "idr": number, readonly "intra_refresh": number, readonly "keyframe_req": number, readonly "loss_max_ppm": number, readonly "loss_mean_ppm": number, readonly "loss_windows": number, readonly "retargets": number, readonly "rfi": number, readonly "rfi_declined": number, readonly "secs": number, readonly "session_id": number, readonly "unrecovered": number, readonly "windows": number }
export const LinkMinute = Schema.Struct({ "abr_backoffs": Schema.Number.annotate({ "description": "Client bitrate asks below the rate the encoder was running. The host cannot see which\nof loss, delay or decode drove them — only that the client's controller retreated.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "abr_max_kbps": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "abr_min_kbps": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "anchor_p": Schema.Number.annotate({ "description": "What the encoder answered an RFI with: a clean P against a surviving reference, or a\nwave that recodes the picture over ~0.5 s.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "egress_kbps": Schema.Number.annotate({ "description": "Sealed wire throughput over the window.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "fec_max_pct": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "fec_min_pct": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "gaps": Schema.Number.annotate({ "description": "Pipeline gaps this host announced: its own stalls, not the network's.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "idr": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "intra_refresh": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "keyframe_req": Schema.Number.annotate({ "description": "Client decode-recovery asks, and the IDRs actually forced (the cooldown coalesces).", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "loss_max_ppm": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "loss_mean_ppm": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "loss_windows": Schema.Number.annotate({ "description": "Of `windows`, those that reported any loss.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "retargets": Schema.Number.annotate({ "description": "Times the encoder's rate actually moved.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "rfi": Schema.Number.annotate({ "description": "Client reference-frame invalidation asks, and those the encoder refused (each refusal\ncosts an IDR).", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "rfi_declined": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "secs": Schema.Number.annotate({ "description": "Seconds this line covers. 60, except the last line of a session.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "session_id": Schema.Number.annotate({ "description": "The `/status` session id, so a line ties to a row.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "unrecovered": Schema.Number.annotate({ "description": "Windows that closed with frames parity could not repair.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "windows": Schema.Number.annotate({ "description": "Client loss reports that closed here. 0 = the client sent none, which is itself a fault.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "One minute of link health for one session. Every field is a delta for the window except\nthe bands, which are min..max over it." })
export type LogEntry = { readonly "level": string, readonly "msg": string, readonly "seq": number, readonly "target": string, readonly "ts_ms": number }
export const LogEntry = Schema.Struct({ "level": Schema.String.annotate({ "description": "`ERROR` | `WARN` | `INFO` | `DEBUG` | `TRACE`." }), "msg": Schema.String.annotate({ "description": "Formatted message; structured fields appended as `key=value`." }), "seq": Schema.Number.annotate({ "description": "Monotonic sequence number (1-based) — pass the last one back as the `after` cursor.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "target": Schema.String, "ts_ms": Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) })
export type Matching = "exact" | "search"
export const Matching = Schema.Literals(["exact", "search"]).annotate({ "description": "How a source finds its games. A new source lands after the last one of its kind, exact first." })
export type MetadataAccepted = { readonly "dropped": number, readonly "entries": number }
export const MetadataAccepted = Schema.Struct({ "dropped": Schema.Number.annotate({ "description": "Values dropped because the host does not store them (non-`http(s)` art, overlong text)." }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "entries": Schema.Number.annotate({ "description": "Entries the host kept." }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) })
export type MetadataRemoved = { readonly "removed": boolean }
export const MetadataRemoved = Schema.Struct({ "removed": Schema.Boolean })
export type MetadataSourceUpdate = { readonly "enabled": boolean, readonly "id": string, readonly "replace": boolean }
export const MetadataSourceUpdate = Schema.Struct({ "enabled": Schema.Boolean, "id": Schema.String, "replace": Schema.Boolean }).annotate({ "description": "One row of `PUT /library/metadata`. The array's order is the new order." })
export type ModeConflict = "separate" | "steal" | "join" | "reject"
export const ModeConflict = Schema.Literals(["separate", "steal", "join", "reject"]).annotate({ "description": "Admission when a new client asks for a different mode than the live\ndisplay. [`super::admission`] runs this before Welcome so `reject` is a\nhandshake error, not a half-built session." })
export type NativeClient = { readonly "access_level"?: string | null, readonly "expires_unix"?: never, readonly "fingerprint": string, readonly "granted_unix"?: never, readonly "grants"?: never, readonly "name": string, readonly "preferred_pad_slot"?: number, readonly "until_disconnect": boolean }
export const NativeClient = Schema.Struct({ "access_level": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "`full` | `controller` | `view` | `custom`. Derived from `grants`;\nabsent only on hosts older than this field." })), "expires_unix": Schema.optionalKey(Schema.Never), "fingerprint": Schema.String.annotate({ "description": "Hex SHA-256 of the client certificate — the stable id." }), "granted_unix": Schema.optionalKey(Schema.Never), "grants": Schema.optionalKey(Schema.Never), "name": Schema.String, "preferred_pad_slot": Schema.optionalKey(Schema.Number.annotate({ "description": "Player slot this device's pads take, 0-based; `null` = whichever comes free.\nSet through `PUT /session/{id}/player` while the device streams.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))), "until_disconnect": Schema.Boolean.annotate({ "description": "The record is dropped when the device's last session ends, rather than at a clock\ntime. `expires_unix` may also be set; whichever comes first ends the grant." }) })
export type NativePairStatus = { readonly "armed": boolean, readonly "enabled": boolean, readonly "expires_in_secs"?: never, readonly "paired_clients": number, readonly "pin"?: string | null }
export const NativePairStatus = Schema.Struct({ "armed": Schema.Boolean, "enabled": Schema.Boolean.annotate({ "description": "True when this process started with `--native`." }), "expires_in_secs": Schema.optionalKey(Schema.Never), "paired_clients": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "pin": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])) }).annotate({ "description": "Native pairing window. Unlike GameStream, the host mints the PIN (SPAKE2\nneeds it client-side first); the console displays it." })
export type PadFrame = { readonly "buttons": number, readonly "declared"?: string | null, readonly "device": string, readonly "left_trigger": number, readonly "ls_x": number, readonly "ls_y": number, readonly "pad": number, readonly "present": boolean, readonly "right_trigger": number, readonly "rs_x": number, readonly "rs_y": number, readonly "slot"?: number, readonly "ts_ms": number }
export const PadFrame = Schema.Struct({ "buttons": Schema.Number.annotate({ "description": "`punktfunk_core::input::gamepad::BTN_*` mask, as applied.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "declared": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "What the client declared at arrival, when it declared one. Differs from `device`\nwhere the build cannot construct that kind and folded it into one it can." })), "device": Schema.String.annotate({ "description": "The virtual controller the host built: `xbox360`, `dualsense`, `steamdeck`, …" }), "left_trigger": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ls_x": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()), "ls_y": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()), "pad": Schema.Number.annotate({ "description": "Wire index — the number this client gave the pad.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "present": Schema.Boolean.annotate({ "description": "`false` is the unplug frame: the host holds no device at this index any more." }), "right_trigger": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "rs_x": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()), "rs_y": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()), "slot": Schema.optionalKey(Schema.Number.annotate({ "description": "Host-wide OS slot — the identity every per-pad host resource is named by\n(mailbox, `SwDeviceCreate` instance, pairing MAC). Absent before the first\nframe builds the device.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds ([`crate::events::HostEvent`] convention).", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "One pad's whole state as the host holds it — the `data:` of one SSE frame.\n\nWhole state, not a delta: a page that attaches mid-press draws what is held without\nreplaying history, and the console derives its event log by diffing consecutive frames." })
export type PairedClient = { readonly "fingerprint": string, readonly "label"?: string | null, readonly "not_after_unix"?: never, readonly "not_before_unix"?: never, readonly "subject"?: string | null }
export const PairedClient = Schema.Struct({ "fingerprint": Schema.String.annotate({ "description": "Lowercase hex SHA-256 of the client certificate DER — the client's stable id here." }), "label": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Operator-assigned display name (`PATCH /clients/{fp}`). The only way to tell two\npaired Moonlight devices apart; absent until somebody names the device." })), "not_after_unix": Schema.optionalKey(Schema.Never), "not_before_unix": Schema.optionalKey(Schema.Never), "subject": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Certificate subject if the DER parses. Do not display as a device name: every\nmoonlight-common-c client self-signs `CN=NVIDIA GameStream Client`, so this names\nthe protocol. [`Self::label`] is the field to show." })) }).annotate({ "description": "A paired (certificate-pinned) Moonlight client." })
export type PendingCeremony = { readonly "fingerprint": string, readonly "peer_ip": string, readonly "uniqueid": string }
export const PendingCeremony = Schema.Struct({ "fingerprint": Schema.String, "peer_ip": Schema.String, "uniqueid": Schema.String }).annotate({ "description": "One pairing handshake parked waiting for its PIN." })
export type PendingDevice = { readonly "access_level"?: string | null, readonly "age_secs": number, readonly "expires_unix"?: never, readonly "fingerprint": string, readonly "granted_unix"?: never, readonly "grants"?: never, readonly "id": number, readonly "name": string, readonly "source": string, readonly "until_disconnect": boolean }
export const PendingDevice = Schema.Struct({ "access_level": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Stored mask's preset. `null` with no stored record — unlike\n[`NativeClient`], where it is always derivable." })), "age_secs": Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "expires_unix": Schema.optionalKey(Schema.Never), "fingerprint": Schema.String.annotate({ "description": "Hex SHA-256 of the device certificate — what approval pins." }), "granted_unix": Schema.optionalKey(Schema.Never), "grants": Schema.optionalKey(Schema.Never), "id": Schema.Number.annotate({ "description": "Approve/deny id. Per-process; entries expire after ~10 minutes.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "name": Schema.String.annotate({ "description": "Client's own name, else fingerprint-derived." }), "source": Schema.String.annotate({ "description": "Where the knock came from: `\"lan\"` or `\"wan\"`. A `\"wan\"` device cannot be admitted by\napprove — arm a PIN bound to its fingerprint instead." }), "until_disconnect": Schema.Boolean.annotate({ "description": "Stored \"this session\" setting if this fingerprint was paired before. `false` if unknown." }) }).annotate({ "description": "Knock awaiting delegated approval (pair here instead of fetching a PIN)." })
export type PendingRequest = { readonly "at": string, readonly "path": string, readonly "reason"?: string | null, readonly "write": boolean }
export const PendingRequest = Schema.Struct({ "at": Schema.String, "path": Schema.String, "reason": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "write": Schema.Boolean }).annotate({ "description": "A path a plugin asked for that the operator has not answered yet." })
export type Plane = "native" | "gamestream"
export const Plane = Schema.Literals(["native", "gamestream"]).annotate({ "description": "Origin plane. Both planes must emit; filtering is the consumer's job." })
export type PlayingApps = { readonly "apps": ReadonlyArray<string> }
export const PlayingApps = Schema.Struct({ "apps": Schema.Array(Schema.String).annotate({ "description": "Lowercased app names, as the voice-chat app list matches them." }) })
export type PluginLogLine = { readonly "level": string, readonly "msg": string, readonly "source": string, readonly "ts_ms": number }
export const PluginLogLine = Schema.Struct({ "level": Schema.String.annotate({ "description": "Normalized by [`crate::log_capture::LogRing::push_remote`]; unknown becomes INFO." }), "msg": Schema.String, "source": Schema.String, "ts_ms": Schema.Number.annotate({ "description": "Unix ms, kept verbatim — see [`crate::log_capture::LogRing::push_remote`].", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) })
export type PluginRegistration = { readonly "category"?: string | null, readonly "hold_timeout_ms"?: never, readonly "holds"?: ReadonlyArray<string>, readonly "title": string, readonly "ui"?: null | { readonly "config"?: boolean, readonly "game"?: boolean, readonly "icon"?: string | null, readonly "page"?: boolean, readonly "port": number, readonly "secret": string }, readonly "version"?: string | null }
export const PluginRegistration = Schema.Struct({ "category": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Plugin kind, not a UI field. Console hides `library` from the nav (those plugins already have\nGame sources); omit the field to keep a nav page." })), "hold_timeout_ms": Schema.optionalKey(Schema.Never), "holds": Schema.optionalKey(Schema.Array(Schema.String).annotate({ "description": "Stages this plugin holds (`game.launching`). The host POSTs the event to `/__hold` on\n`ui.port` and waits for a 2xx. Needs `ui`." })), "title": Schema.String, "ui": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "config": Schema.optionalKey(Schema.Boolean.annotate({ "description": "Serves `GET/PUT /__config`, the plugin-wide settings form." })), "game": Schema.optionalKey(Schema.Boolean.annotate({ "description": "Serves `GET/PUT /__game?entry=<id>`, a tab on each library entry's page." })), "icon": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "page": Schema.optionalKey(Schema.Boolean.annotate({ "description": "Serves a page the console opens and lists in the nav. Absent means yes, as before the flag." })), "port": Schema.Number.annotate({ "description": "Loopback only — the host dials `127.0.0.1:<port>`; a registration cannot carry a hostname.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "secret": Schema.String.annotate({ "description": "Per-boot; the console proxy presents it as `Authorization: Bearer`. Rotated on plugin restart." }) }).annotate({ "description": "Absent `ui` is a live listing with no nav entry — not an error." })], { mode: "oneOf" })), "version": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])) })
export type PluginUiPublic = { readonly "config": boolean, readonly "game": boolean, readonly "icon"?: string | null, readonly "page": boolean, readonly "port": number }
export const PluginUiPublic = Schema.Struct({ "config": Schema.Boolean.annotate({ "description": "See [`PluginUi::config`]." }), "game": Schema.Boolean.annotate({ "description": "See [`PluginUi::game`]." }), "icon": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "page": Schema.Boolean.annotate({ "description": "See [`PluginUi::page`]." }), "port": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "Secret-free UI view for the listing. The secret never goes here." })
export type PortMap = { readonly "audio": number, readonly "control": number, readonly "http": number, readonly "https": number, readonly "mgmt": number, readonly "rtsp": number, readonly "video": number }
export const PortMap = Schema.Struct({ "audio": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "control": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "http": Schema.Number.annotate({ "description": "nvhttp plain HTTP (serverinfo, pairing).", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "https": Schema.Number.annotate({ "description": "nvhttp mutual-TLS HTTPS (post-pairing).", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "mgmt": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "rtsp": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "video": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "Ports a client needs. Moonlight derives stream ports from HTTP; a control pane should not." })
export type Position = { readonly "x": number, readonly "y": number }
export const Position = Schema.Struct({ "x": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()), "y": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()) }).annotate({ "description": "Desktop-space offset (top-left origin)." })
export type PrepCmd = { readonly "do": string, readonly "undo"?: string | null }
export const PrepCmd = Schema.Struct({ "do": Schema.String.annotate({ "description": "Command run before launch. Same recipe and ownership checks as hook `run`; stdin is `{}`." }), "undo": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "After session end. Skipped when its `do` failed (it never took effect)." })) }).annotate({ "description": "Per-app prep (Sunshine `prep-cmd` parity): `do` runs synchronously before launch;\n`undo` runs at session end, reverse order, best-effort, including panic-unwind ([`PrepGuard`])." })
export type Preset = "custom" | "default" | "gaming-rig" | "shared-desktop" | "hotdesk" | "workstation"
export const Preset = Schema.Literals(["custom", "default", "gaming-rig", "shared-desktop", "hotdesk", "workstation"]).annotate({ "description": "Named bundle of the fields below. `Custom` uses the explicit fields;\nany other preset ignores them and expands ([`DisplayPolicy::effective`])." })
export type ProviderRemoved = { readonly "removed": number }
export const ProviderRemoved = Schema.Struct({ "removed": Schema.Number.check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) })
export type ProviderRunningAccepted = { readonly "matched": number, readonly "ttl_s": number, readonly "unknown": number }
export const ProviderRunningAccepted = Schema.Struct({ "matched": Schema.Number.check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ttl_s": Schema.Number.annotate({ "description": "Seconds this report stays authoritative without being restated.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "unknown": Schema.Number.annotate({ "description": "Ignored because no such entry exists (report raced a reconcile)." }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) })
export type ReleaseDisplayRequest = { readonly "slot"?: never }
export const ReleaseDisplayRequest = Schema.Struct({ "slot": Schema.optionalKey(Schema.Never) }).annotate({ "description": "Request body for `releaseDisplay`." })
export type ReleaseDisplayResult = { readonly "released": number }
export const ReleaseDisplayResult = Schema.Struct({ "released": Schema.Number.check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) })
export type Remedy = { readonly "command"?: string | null, readonly "relogin_required": boolean, readonly "text": string }
export const Remedy = Schema.Struct({ "command": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "relogin_required": Schema.Boolean.annotate({ "description": "Fix takes effect only after logout. `systemd --user` keeps the group set it started with." }), "text": Schema.String.annotate({ "description": "English fallback. The console overrides it by check id." }) }).annotate({ "description": "Operator action. Copy-paste only: the host is unprivileged and does not run the command.\nJoining `punktfunk` is opt-in — write on the vhci `attach` node materialises arbitrary USB." })
export type RenameClient = { readonly "label"?: string | null }
export const RenameClient = Schema.Struct({ "label": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Display name. `null` or empty/whitespace clears it (listed by fingerprint alone).\n\nScrubbed with the native-plane sanitizer: control characters and Unicode bidi\noverrides stripped (one device could impersonate another in this list), whitespace\ncollapsed, capped at 64 characters." })) }).annotate({ "description": "Body of `PATCH /clients/{fingerprint}` — the device's display name." })
export type RunningTitle = { readonly "external_id": string, readonly "pid"?: never }
export const RunningTitle = Schema.Struct({ "external_id": Schema.String.annotate({ "description": "Same key the reconcile payload uses." }), "pid": Schema.optionalKey(Schema.Never) })
export type RuntimeRequest = { readonly "enabled": boolean }
export const RuntimeRequest = Schema.Struct({ "enabled": Schema.Boolean })
export type RuntimeView = { readonly "detail"?: string | null, readonly "enabled": boolean, readonly "installed": boolean, readonly "principal"?: string | null, readonly "running": boolean, readonly "unit": string }
export const RuntimeView = Schema.Struct({ "detail": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "enabled": Schema.Boolean, "installed": Schema.Boolean, "principal": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Windows: the account the task runs as." })), "running": Schema.Boolean, "unit": Schema.String.annotate({ "description": "systemd unit or scheduled-task name." }) })
export type ScannerInfo = { readonly "enabled": boolean, readonly "entries"?: number, readonly "id": string, readonly "label": string, readonly "origin": "builtin" | "plugin", readonly "provider"?: string | null }
export const ScannerInfo = Schema.Struct({ "enabled": Schema.Boolean, "entries": Schema.optionalKey(Schema.Union([Schema.Number.check(Schema.isInt()).check(Schema.makeFilterGroup([Schema.isFinite(), Schema.isGreaterThanOrEqualTo(0)], { "description": "Titles this source currently contributes. `None` on `Builtin` (counting would walk launcher files)." }))])), "id": Schema.String.annotate({ "description": "Entry `store`, provider id, and store claim. One string, so a disabled\ntoggle survives the store's plugin taking over." }), "label": Schema.String, "origin": Schema.Literals(["builtin", "plugin"]).annotate({ "description": "Always `plugin` on this host. `Builtin` remains so OpenAPI still names it for N-1 consoles." }), "provider": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Provider id. `None` only for a `Builtin` source an N-1 host still reports." })) }).annotate({ "description": "One game source and its enable state — the console toggle row." })
export type ScannerToggle = { readonly "enabled": boolean }
export const ScannerToggle = Schema.Struct({ "enabled": Schema.Boolean })
export type SessionAccess = { readonly "grants": number, readonly "level": string }
export const SessionAccess = Schema.Struct({ "grants": Schema.Number.annotate({ "description": "What now governs the session — the request ANDed with the pairing's mask.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "level": Schema.String.annotate({ "description": "`full` | `controller` | `view` | `custom`, derived from `grants`." }) })
export type SessionAccessRequest = { readonly "grants"?: number, readonly "level"?: string | null }
export const SessionAccessRequest = Schema.Struct({ "grants": Schema.optionalKey(Schema.Number.annotate({ "description": "Exact `GRANT_*` mask, for a level the three names do not cover. Reserved bits are 400.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))), "level": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "`full` | `controller` | `view`. Ignored when `grants` is present." })) })
export type SessionAudioRequest = { readonly "muted": boolean }
export const SessionAudioRequest = Schema.Struct({ "muted": Schema.Boolean.annotate({ "description": "`true` stops audio leaving for this session." }) })
export type SessionEndReason = "local" | "game_exited" | "host_ended" | "host_error" | "lost" | "stopped_by_operator"
export const SessionEndReason = Schema.Literals(["local", "game_exited", "host_ended", "host_error", "lost", "stopped_by_operator"]).annotate({ "description": "Why a session ended, in the client's own words\n([`punktfunk_core::client::PunktfunkEndReason`]) so both ends name the same end the\nsame way. The bytes match that ABI where the two overlap.\n\n[`Self::StoppedByOperator`] is the one a client cannot see: the host closes it as\ncode 0, which every client reads as `host_ended`." })
export type SessionPlayer = { readonly "pads": ReadonlyArray<number>, readonly "reserved": boolean, readonly "slot"?: number }
export const SessionPlayer = Schema.Struct({ "pads": Schema.Array(Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))).annotate({ "description": "OS pad slots the session holds right now. A pad already built keeps its slot\nuntil it re-plugs, so this can still name the old player for a moment." }), "reserved": Schema.Boolean.annotate({ "description": "`false` = another live session asked for that slot first and keeps it; this\nsession stays on the first-free claim." }), "slot": Schema.optionalKey(Schema.Number.annotate({ "description": "The pick now stored for this session.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))) })
export type SessionPlayerRequest = { readonly "slot"?: number }
export const SessionPlayerRequest = Schema.Struct({ "slot": Schema.optionalKey(Schema.Number.annotate({ "description": "Player slot, 0-based: `0` is Player 1. Omit or `null` to hand the session\nback to the first-free claim.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))) })
export type SessionRef = { readonly "client": string, readonly "fingerprint"?: string | null, readonly "hdr": boolean, readonly "id": number, readonly "mode": string, readonly "plane": "native" | "gamestream", readonly "preset"?: null | { readonly "id": string, readonly "name": string } }
export const SessionRef = Schema.Struct({ "client": Schema.String.annotate({ "description": "Cert-fingerprint prefix, or peer IP for an anonymous client — not [`ClientRef::name`]." }), "fingerprint": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "The device's full stable id, for a hook filter. Absent for an anonymous client." })), "hdr": Schema.Boolean, "id": Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "mode": Schema.String.annotate({ "description": "`WxH@Hz`, e.g. `\"3840x2160@120\"`." }), "plane": Schema.Literals(["native", "gamestream"]).annotate({ "description": "Which plane serves it, as `stream.*` and `game.*` also report." }), "preset": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "id": Schema.String, "name": Schema.String }).annotate({ "description": "See [`ClientRef::preset`]." })], { mode: "oneOf" })) }).annotate({ "description": "Plane-neutral A/V session (distinct from a video [`StreamRef`])." })
export type SetGpuPreference = { readonly "gpu_id"?: string | null, readonly "mode": string }
export const SetGpuPreference = Schema.Struct({ "gpu_id": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Required for `manual`: a currently listed GPU `id`." })), "mode": Schema.String.annotate({ "description": "`auto` (env pin, else max dedicated VRAM) or `manual`." }) })
export type SettingApply = "now" | "next_session" | "restart"
export const SettingApply = Schema.Literals(["now", "next_session", "restart"])
export type SettingGroup = "streaming" | "video" | "audio" | "input" | "network" | "game_mode" | "session" | "system"
export const SettingGroup = Schema.Literals(["streaming", "video", "audio", "input", "network", "game_mode", "session", "system"])
export type SettingKind = "bool" | "int" | "decimal" | "enum" | "text" | "list"
export const SettingKind = Schema.Literals(["bool", "int", "decimal", "enum", "text", "list"])
export type SettingSource = "default" | "store" | "env" | "flag"
export const SettingSource = Schema.Literals(["default", "store", "env", "flag"])
export type SourceInput = { readonly "public_key"?: string | null, readonly "url": string }
export const SourceInput = Schema.Struct({ "public_key": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "`ed25519:<base64>`. Omitted ⇒ an unsigned source (accepted, flagged everywhere)." })), "url": Schema.String })
export type SourceView = { readonly "builtin": boolean, readonly "entry_count": number, readonly "error"?: string | null, readonly "fetched_at"?: never, readonly "name": string, readonly "public_key"?: string | null, readonly "signed": boolean, readonly "stale": boolean, readonly "url": string }
export const SourceView = Schema.Struct({ "builtin": Schema.Boolean.annotate({ "description": "Built-in `unom`: not editable, not removable; only its entries may be `verified`." }), "entry_count": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "error": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "fetched_at": Schema.optionalKey(Schema.Never), "name": Schema.String, "public_key": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "signed": Schema.Boolean.annotate({ "description": "Unsigned sources still serve; the console flags them." }), "stale": Schema.Boolean.annotate({ "description": "Last refresh missed; entries still install — the pin travelled with the entry." }), "url": Schema.String })
export type StageTiming = { readonly "name": string, readonly "p50_us": number, readonly "p99_us": number }
export const StageTiming = Schema.Struct({ "name": Schema.String.annotate({ "description": "Pipeline order, named per path. Linux native: `queue capture submit encode send`.\nWindows driver: `pool encode ipc copy send`, or `driver copy send` from a driver that\ndoes not stamp its slots. GameStream: `capture encode packetize send send_spread`, with\nthe same two driver sets on Windows." }), "p50_us": Schema.Number.annotate({ "format": "float" }).check(Schema.isFinite()), "p99_us": Schema.Number.annotate({ "format": "float" }).check(Schema.isFinite()) }).annotate({ "description": "One stage's p50/p99 in an aggregation window (microseconds)." })
export type State = "running" | "done" | "failed"
export const State = Schema.Literals(["running", "done", "failed"])
export type StatsStatus = { readonly "armed": boolean, readonly "elapsed_ms": number, readonly "kind": string, readonly "sample_count": number, readonly "started_unix_ms": number }
export const StatsStatus = Schema.Struct({ "armed": Schema.Boolean, "elapsed_ms": Schema.Number.annotate({ "description": "Host monotonic elapsed ms (`0` if idle). Do not subtract `started_unix_ms`\nfrom the console's wall clock — that clock may be skewed.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "kind": Schema.String.annotate({ "description": "`\"native\" | \"gamestream\"`, or `\"\"` if idle." }), "sample_count": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "started_unix_ms": Schema.Number.annotate({ "description": "Unix start of the in-progress capture (`0` if idle).", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "In-progress capture, as the management API reports it." })
export type SubmitPin = { readonly "fingerprint": string, readonly "label"?: string | null, readonly "peer_ip": string, readonly "pin": string, readonly "uniqueid": string }
export const SubmitPin = Schema.Struct({ "fingerprint": Schema.String, "label": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Name for this device, scrubbed like `PATCH /clients/{fp}` and stored once the pairing\ncompletes. Every Moonlight client names itself the same, so without one the device is\nlisted by fingerprint until somebody renames it." })), "peer_ip": Schema.String, "pin": Schema.String, "uniqueid": Schema.String }).annotate({ "description": "PIN plus the exact ceremony selected from `GET /pair`." })
export type TokenGrant = { readonly "expires_at": number, readonly "fingerprint": string, readonly "token": string }
export const TokenGrant = Schema.Struct({ "expires_at": Schema.Number.annotate({ "description": "Unix seconds.", "format": "int64" }).check(Schema.isInt()), "fingerprint": Schema.String.annotate({ "description": "The device's own fingerprint, so a client can show which identity it is using." }), "token": Schema.String.annotate({ "description": "Present as `Authorization: Bearer <token>`." }) })
export type TokenRequest = { readonly "device_key": string, readonly "nonce": string, readonly "signature": string }
export const TokenRequest = Schema.Struct({ "device_key": Schema.String.annotate({ "description": "Base64 SPKI of the device's P-256 public key — the same bytes it paired with, whose\nSHA-256 the host stored." }), "nonce": Schema.String.annotate({ "description": "The nonce from `challenge`." }), "signature": Schema.String.annotate({ "description": "Base64 ECDSA-P256-SHA256 signature, ASN.1 DER, over the same message the control stream\nuses: context, the host's identity fingerprint, then the nonce." }) }).annotate({ "description": "`POST /auth/device/token` — what a paired browser presents." })
export type Topology = "auto" | "extend" | "primary" | "exclusive"
export const Topology = Schema.Literals(["auto", "extend", "primary", "exclusive"]).annotate({ "description": "Host topology while managed virtual displays are up." })
export type UiCredential = { readonly "port": number, readonly "secret": string }
export const UiCredential = Schema.Struct({ "port": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "secret": Schema.String }).annotate({ "description": "The only shape that returns a secret. The console BFF denylists this lookup from the browser." })
export type UninstallRequest = { readonly "pkg": string }
export const UninstallRequest = Schema.Struct({ "pkg": Schema.String })
export type UnpairAllResult = { readonly "unpaired": number }
export const UnpairAllResult = Schema.Struct({ "unpaired": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "Bulk-unpair result, shared by `/clients` and `/native/clients`.\n\nA count, not 204: unpair-everything is idempotent, and the operator still\nneeds to know whether that was three devices or none." })
export type UpdateJobInfo = { readonly "received_bytes": number, readonly "stage": string, readonly "started_unix": number, readonly "target_version": string, readonly "total_bytes"?: never }
export const UpdateJobInfo = Schema.Struct({ "received_bytes": Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "stage": Schema.String.annotate({ "description": "`downloading` | `verifying` | `applying` | `restarting`." }), "started_unix": Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "target_version": Schema.String, "total_bytes": Schema.optionalKey(Schema.Never) }).annotate({ "description": "In-process apply, or a leftover installer that has not resolved yet." })
export type UpdateManifestInfo = { readonly "notes_url": string, readonly "published_at": string, readonly "serial": number, readonly "stale": boolean, readonly "version": string }
export const UpdateManifestInfo = Schema.Struct({ "notes_url": Schema.String.annotate({ "description": "Forge-pinned by the manifest validator." }), "published_at": Schema.String.annotate({ "description": "RFC-3339; display only, never compared." }), "serial": Schema.Number.annotate({ "description": "Unix seconds; monotonic per channel.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "stale": Schema.Boolean.annotate({ "description": "Last verified manifest older than 45 days." }), "version": Schema.String })
export type UpdateNativeAccess = { readonly "clear_expiry"?: boolean | null, readonly "expires_in_secs"?: never, readonly "grants"?: never, readonly "until_disconnect"?: boolean | null }
export const UpdateNativeAccess = Schema.Struct({ "clear_expiry": Schema.optionalKey(Schema.Union([Schema.Boolean, Schema.Null]).annotate({ "description": "`true` makes access permanent. Mutually exclusive with `expires_in_secs` (400)." })), "expires_in_secs": Schema.optionalKey(Schema.Never), "grants": Schema.optionalKey(Schema.Never), "until_disconnect": Schema.optionalKey(Schema.Union([Schema.Boolean, Schema.Null]).annotate({ "description": "Drop the record once the device's last session ends. Omit to keep the current setting." })) }).annotate({ "description": "Partial PATCH of a paired device's grants/expiry. Omitted fields keep their\ncurrent value." })
export type UpdateResultInfo = { readonly "error"?: string | null, readonly "finished_unix": number, readonly "from": string, readonly "log_path"?: string | null, readonly "ok": boolean, readonly "stage"?: string | null, readonly "staged"?: boolean, readonly "to": string }
export const UpdateResultInfo = Schema.Struct({ "error": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "finished_unix": Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "from": Schema.String, "log_path": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "ok": Schema.Boolean, "stage": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Failed stage; absent on success." })), "staged": Schema.optionalKey(Schema.Boolean.annotate({ "description": "Applied; activates on the next reboot (rpm-ostree)." })), "to": Schema.String }).annotate({ "description": "Last apply outcome. Survives the host's own restart." })
export type WebTransportInfo = { readonly "allow_pooling": boolean, readonly "cert_hash_sha256": string, readonly "cert_hash_sig"?: string | null, readonly "expires_at": number, readonly "host_cert_der"?: string | null, readonly "port": number }
export const WebTransportInfo = Schema.Struct({ "allow_pooling": Schema.Boolean.annotate({ "description": "`allowPooling: true` is a `TypeError` when combined with `serverCertificateHashes`, so a\nclient must pass this. Stated here because a browser that ignores it fails at Web PKI\nvalidation with no useful error." }), "cert_hash_sha256": Schema.String.annotate({ "description": "Lowercase hex SHA-256 of the leaf certificate DER — the bytes that go in\n`serverCertificateHashes[0].value`." }), "cert_hash_sig": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Hex ECDSA-P256-SHA256 signature (ASN.1 DER) by the host's long-lived native identity over\n`\"punktfunk-wt-cert-v1:\" + cert_hash_sha256`. Absent when that identity is the legacy RSA\npair. A browser that has paired MUST check this; one that has not cannot, and does not." })), "expires_at": Schema.Number.annotate({ "description": "Unix seconds. Past this the certificate has rotated and the hash above is stale; fetch\nagain rather than cache. Always under two weeks out — the spec refuses anything longer.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "host_cert_der": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Base64 DER of the native identity's leaf certificate — the key that verifies\n`cert_hash_sig`. A browser hashes it and compares with the fingerprint it stored at\npairing; trusting it without that check would defeat the whole exercise." })), "port": Schema.Number.annotate({ "description": "UDP port the plane listens on. Not the management port, and not the native plane's.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "Everything `new WebTransport(url, { serverCertificateHashes })` needs." })
export type WorkspacePlacement = "own" | "current"
export const WorkspacePlacement = Schema.Literals(["own", "current"]).annotate({ "description": "Where a library launch's windows open on the streamed head.\n\nDefault `own`: the player gets the game on an empty workspace instead of\nthe operator's desk. Honoured only by the backends that can place a launch\n(`claim_workspace`); everywhere else a launch is always `current`.\n\n[`DisplayPolicy`] field, not part of [`EffectivePolicy`]: a preset never\nclobbers it. A library entry's own `on_window.workspace` outranks it." })
export type AccessRequest = { readonly "paths": ReadonlyArray<AccessPathRequest>, readonly "reason"?: string | null }
export const AccessRequest = Schema.Struct({ "paths": Schema.Array(AccessPathRequest), "reason": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Why the plugin wants it, in the plugin's words. Optional, sanitized, ≤120 chars." })) })
export type ActionList = { readonly "actions": ReadonlyArray<ActionInfo> }
export const ActionList = Schema.Struct({ "actions": Schema.Array(ActionInfo) })
export type DisplayStateResponse = { readonly "displays": ReadonlyArray<ApiDisplayInfo> }
export const DisplayStateResponse = Schema.Struct({ "displays": Schema.Array(ApiDisplayInfo) })
export type MonitorsResponse = { readonly "compositor"?: string | null, readonly "error"?: string | null, readonly "monitors": ReadonlyArray<ApiMonitorInfo>, readonly "pin_supported": boolean, readonly "pinned"?: string | null }
export const MonitorsResponse = Schema.Struct({ "compositor": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Enumeration source (`kwin`, `mutter`, `windows`), when resolved." })), "error": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Enumeration failure. `None` with an empty list means the host has no heads." })), "monitors": Schema.Array(ApiMonitorInfo).annotate({ "description": "Heads, ordered left-to-right by desktop position." }), "pin_supported": Schema.Boolean.annotate({ "description": "True when this build can stream a chosen physical head.\n\nEnumeration and capture are separate. Off Linux, heads are listed but there is no\nmirror backend (`vdisplay::open` has no Windows arm; `pf-capture` only has\n`open_idd_push`). The console treats `false` as a read-only picker." }), "pinned": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Configured pin, even when it matches no head (console can show a dangling pin)." })) })
export type GpuState = { readonly "active"?: null | ApiActiveGpu, readonly "encoder_pin"?: string | null, readonly "env_override"?: string | null, readonly "gpus": ReadonlyArray<ApiGpu>, readonly "mode": string, readonly "preferred_available": boolean, readonly "preferred_id"?: string | null, readonly "preferred_name"?: string | null, readonly "selected"?: null | ApiSelectedGpu }
export const GpuState = Schema.Struct({ "active": Schema.optionalKey(Schema.Union([Schema.Null, ApiActiveGpu], { mode: "oneOf" })), "encoder_pin": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "`PUNKTFUNK_ENCODER` when pinned (`qsv` / `nvenc` / …, not `auto`). A vendor mismatch is\noverridden at session open (adapter wins); the console uses this to flag a stale pin." })), "env_override": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "`PUNKTFUNK_RENDER_ADAPTER` when set. Honoured in `auto`; a manual pick overrides it." })), "gpus": Schema.Array(ApiGpu), "mode": Schema.String.annotate({ "description": "`auto` or `manual`." }), "preferred_available": Schema.Boolean, "preferred_id": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Stored manual pick; retained in `auto` so the console can switch back. May name an absent GPU." })), "preferred_name": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Label for the stored pick, kept even when that GPU is absent." })), "selected": Schema.optionalKey(Schema.Union([Schema.Null, ApiSelectedGpu], { mode: "oneOf" })) })
export type AudioPolicy = { readonly "sessions"?: AudioSessions }
export const AudioPolicy = Schema.Struct({ "sessions": Schema.optionalKey(AudioSessions) }).annotate({ "description": "Audio while this title runs: which of the sessions on its display hear it.\n`all` is the same as no policy and is stored as none." })
export type LocalSummary = { readonly "audio_streaming": boolean, readonly "client_name"?: string | null, readonly "conflicts"?: ReadonlyArray<string>, readonly "games"?: ReadonlyArray<string>, readonly "kept_displays": number, readonly "native_paired_clients": number, readonly "paired_clients": number, readonly "pending_approvals": number, readonly "pin_pending": boolean, readonly "session"?: null | { readonly "capture"?: null | { readonly "backend_opened"?: string | null, readonly "class": string, readonly "cooldown_remaining_ms"?: never, readonly "current_stage"?: string | null, readonly "detached": number, readonly "dropped_total": number, readonly "encoder_state"?: string | null, readonly "episodes_suppressed": number, readonly "evidence"?: string | null, readonly "last_episode"?: null | { readonly "consecutive_failures": number, readonly "cooldown_ms": number, readonly "recovered": boolean, readonly "stages": ReadonlyArray<CaptureStage>, readonly "stall_class": string, readonly "took_ms": number }, readonly "late_frames": boolean, readonly "present_to_arrival_ms"?: never, readonly "published_total": number, readonly "source_gap_ms": number, readonly "stall_class"?: string | null }, readonly "fps": number, readonly "height": number, readonly "width": number }, readonly "version": string, readonly "video_streaming": boolean }
export const LocalSummary = Schema.Struct({ "audio_streaming": Schema.Boolean.annotate({ "description": "True while audio is streaming on either plane (same rule as `video_streaming`)." }), "client_name": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "First native session's display name (trust-store, else connect-time). `null` when idle, nameless, or GameStream." })), "conflicts": Schema.optionalKey(Schema.Array(Schema.String).annotate({ "description": "Other GameStream hosts on this machine, detected at startup. Running one alongside is unsupported." })), "games": Schema.optionalKey(Schema.Array(Schema.String).annotate({ "description": "Compact labels (`Hades`, `Hades (closing in 4:12)`). Countdown means the client is gone and the host will end the game when the window closes." })), "kept_displays": Schema.Number.annotate({ "description": "Lingering or pinned virtual displays with no live session. Active (in-use) displays are not counted.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "native_paired_clients": Schema.Number.annotate({ "description": "Native-plane pairing count.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "paired_clients": Schema.Number.annotate({ "description": "GameStream paired-cert count.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "pending_approvals": Schema.Number.annotate({ "description": "Native pairing knocks awaiting the operator's approval (count only).", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "pin_pending": Schema.Boolean.annotate({ "description": "GameStream pairing is waiting for a PIN." }), "session": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "capture": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "backend_opened": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "The backend the driver opened: `nvenc` / `amf` / `qsv` / `pyrowave`. Absent as above." })), "class": Schema.String.annotate({ "description": "`healthy` / `idle` / `suspect` / `stalled` / `recovering` / `rebuilding` / `secure_desktop`." }), "cooldown_remaining_ms": Schema.optionalKey(Schema.Never), "current_stage": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "The recovery stage running now, while an episode is open." })), "detached": Schema.Number.annotate({ "description": "Encode threads the driver abandoned after a wedge; two opens the driver cycle.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "dropped_total": Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "encoder_state": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "The driver encoder's own state word: `closed` / `open` / `encoding` / `wedged`.\nAbsent until the session's first `SET_ENCODE`." })), "episodes_suppressed": Schema.Number.annotate({ "description": "Stalled verdicts refused for budget or cooldown since the last episode.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "evidence": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Activity evidence behind the verdict: `input` / `canary`." })), "last_episode": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "consecutive_failures": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "cooldown_ms": Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "recovered": Schema.Boolean, "stages": Schema.Array(CaptureStage).annotate({ "description": "The rungs run, in ladder order." }), "stall_class": Schema.String, "took_ms": Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "The last closed recovery episode." })], { mode: "oneOf" })), "late_frames": Schema.Boolean.annotate({ "description": "`present_to_arrival_ms` is past the classifier's bound: frames come late rather than not\nat all. Reported only — no recovery rung fires on it." }), "present_to_arrival_ms": Schema.optionalKey(Schema.Never), "published_total": Schema.Number.annotate({ "description": "Access units the driver published, and frames it dropped at its encode pool.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "source_gap_ms": Schema.Number.annotate({ "description": "Time since the last real source frame.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "stall_class": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "When `class` is `stalled`: `worker` / `encoder` / `presentation` / `driver`." })) }).annotate({ "description": "Live capture health (Windows IDD-push, native plane). Absent on GameStream, on Linux,\nand until the video loop's first publish." })], { mode: "oneOf" })), "fps": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "height": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "width": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "GameStream launch if present, else the first live native session. `null` when idle." })], { mode: "oneOf" })), "version": Schema.String.annotate({ "description": "Host version (mirrors `/health`)." }), "video_streaming": Schema.Boolean.annotate({ "description": "Video streaming on either plane. The GameStream flag alone misses native sessions." }) }).annotate({ "description": "Tray snapshot for loopback: counts, booleans, and `client_name`.\nUnauthenticated; `require_auth` admits loopback only (the tray cannot read the bearer file)." })
export type DecideRequest = { readonly "decision": DecisionInput, readonly "path": string, readonly "write"?: boolean | null }
export const DecideRequest = Schema.Struct({ "decision": DecisionInput, "path": Schema.String.annotate({ "description": "The path as it appears in the request or grant list." }), "write": Schema.optionalKey(Schema.Union([Schema.Boolean, Schema.Null]).annotate({ "description": "With `allow`: grant the path directly, pending request or not, read-write when true.\nA file grants its folder. The same refusals apply as to a request." })) })
export type MetadataEntryInput = { readonly "art"?: { readonly "header"?: string | null, readonly "hero"?: string | null, readonly "logo"?: string | null, readonly "portrait"?: string | null }, readonly "id": string, readonly "meta"?: GameMeta }
export const MetadataEntryInput = Schema.Struct({ "art": Schema.optionalKey(Schema.Struct({ "header": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Steam `header.jpg` — the universal fallback." })), "hero": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Steam `library_hero.jpg`." })), "logo": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Steam `logo.png`." })), "portrait": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Steam `library_600x900.jpg`." })) }).annotate({ "description": "`http(s)` URLs only; the host fetches and keeps them like a provider's CDN art." })), "id": Schema.String.annotate({ "description": "Library id, as `GET /library` lists it (`steam:570`, `custom:3f9a0c1b2d4e`)." }), "meta": Schema.optionalKey(GameMeta) }).annotate({ "description": "One entry in a source's push." })
export type SessionSettings = { readonly "disconnect_grace_seconds"?: number, readonly "game_on_new_launch"?: GameOnNewLaunch, readonly "game_on_session_end"?: GameOnSessionEnd, readonly "session_on_game_exit"?: boolean, readonly "version"?: number }
export const SessionSettings = Schema.Struct({ "disconnect_grace_seconds": Schema.optionalKey(Schema.Number.annotate({ "description": "Ignored unless `game_on_session_end` is `Always`.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))), "game_on_new_launch": Schema.optionalKey(GameOnNewLaunch), "game_on_session_end": Schema.optionalKey(GameOnSessionEnd), "session_on_game_exit": Schema.optionalKey(Schema.Boolean), "version": Schema.optionalKey(Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))) })
export type LogPage = { readonly "dropped": boolean, readonly "entries": ReadonlyArray<LogEntry>, readonly "next": number }
export const LogPage = Schema.Struct({ "dropped": Schema.Boolean.annotate({ "description": "Entries between `after` and the first returned one were already evicted." }), "entries": Schema.Array(LogEntry), "next": Schema.Number.annotate({ "description": "Last returned seq, or the request's `after` when the page is empty.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) })
export type MetadataSourceInfo = { readonly "enabled": boolean, readonly "entries": number, readonly "id": string, readonly "matching": Matching, readonly "replace": boolean }
export const MetadataSourceInfo = Schema.Struct({ "enabled": Schema.Boolean, "entries": Schema.Number.annotate({ "description": "Entries the source has something for." }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "id": Schema.String.annotate({ "description": "The source's plugin id." }), "matching": Matching, "replace": Schema.Boolean.annotate({ "description": "\"Use for every game\": this source's art beats the entry's own. Art only." }) }).annotate({ "description": "One Art & Metadata source as the console lists it, in the operator's order." })
export type PairingStatus = { readonly "pending": ReadonlyArray<PendingCeremony>, readonly "pin_pending": boolean }
export const PairingStatus = Schema.Struct({ "pending": Schema.Array(PendingCeremony).annotate({ "description": "Parked ceremonies. Echo this identity in the submit so the PIN addresses the\nceremony the operator saw, not a later arrival." }), "pin_pending": Schema.Boolean }).annotate({ "description": "Pairing-flow status." })
export type PluginAccessSnapshot = { readonly "denied": ReadonlyArray<string>, readonly "grants": ReadonlyArray<Grant>, readonly "pending": ReadonlyArray<PendingRequest>, readonly "plugin": string }
export const PluginAccessSnapshot = Schema.Struct({ "denied": Schema.Array(Schema.String), "grants": Schema.Array(Grant), "pending": Schema.Array(PendingRequest), "plugin": Schema.String }).annotate({ "description": "What an API read returns for one plugin: its entry plus its pending rows." })
export type ClientRef = { readonly "fingerprint"?: string | null, readonly "name": string, readonly "plane": Plane, readonly "preset"?: null | { readonly "id": string, readonly "name": string } }
export const ClientRef = Schema.Struct({ "fingerprint": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "name": Schema.String.annotate({ "description": "Display name: the trust-store name (a console rename wins), else the name the client\nsent. On the compat plane it is the name the operator gave the device, since Moonlight\nsends none. Empty when there is no name at all." }), "plane": Plane, "preset": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "id": Schema.String, "name": Schema.String }).annotate({ "description": "The preset the client dialled with. Absent for plain settings and on GameStream." })], { mode: "oneOf" })) })
export type DeviceRef = { readonly "fingerprint": string, readonly "name": string, readonly "plane": Plane }
export const DeviceRef = Schema.Struct({ "fingerprint": Schema.String, "name": Schema.String.annotate({ "description": "Pairing-store copy, already sanitized." }), "plane": Plane })
export type GameRefPayload = { readonly "app"?: string | null, readonly "client": string, readonly "fingerprint"?: string | null, readonly "plane": Plane, readonly "preset"?: null | { readonly "id": string, readonly "name": string }, readonly "store"?: string | null, readonly "title": string }
export const GameRefPayload = Schema.Struct({ "app": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Store-qualified id (`steam:570`). Absent for an operator-typed GameStream `apps.json` command." })), "client": Schema.String, "fingerprint": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Stable id of the device that launched it; the filter handle a name cannot be." })), "plane": Plane, "preset": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "id": Schema.String, "name": Schema.String }).annotate({ "description": "The preset of the session that launched it. See [`ClientRef::preset`]." })], { mode: "oneOf" })), "store": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "`steam`, `heroic`, `custom`, … when known." })), "title": Schema.String })
export type HookEntry = { readonly "debounce_ms"?: number, readonly "filter"?: null | { readonly "app"?: string | null, readonly "client"?: string | null, readonly "fingerprint"?: string | null, readonly "plane"?: null | Plane, readonly "preset"?: string | null }, readonly "hmac_secret_file"?: string | null, readonly "hold"?: boolean, readonly "on": string, readonly "run"?: string | null, readonly "timeout_s"?: number, readonly "webhook"?: string | null }
export const HookEntry = Schema.Struct({ "debounce_ms": Schema.optionalKey(Schema.Number.annotate({ "description": "Minimum interval between firings, in milliseconds. 0 = fire every time. A `hold` ignores it.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))), "filter": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "app": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Launched app id/title (`stream.*` events)." })), "client": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Client/device name (`session.*`: the Dashboard's short client label)." })), "fingerprint": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Certificate fingerprint (hex, case-insensitive)." })), "plane": Schema.optionalKey(Schema.Union([Schema.Null, Plane], { mode: "oneOf" })), "preset": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "The dialled settings preset, by id or name (`client.*`, `session.*`, `stream.*`, `game.*`)." })) }).annotate({ "description": "Exact-match constraints; every present field must match." })], { mode: "oneOf" })), "hmac_secret_file": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "HMAC secret file (`X-Punktfunk-Signature: sha256=<hex>`). Warns if world-readable." })), "hold": Schema.optionalKey(Schema.Boolean.annotate({ "description": "The launch waits for this hook, up to `timeout_s`. Only with `on: game.launching`." })), "on": Schema.String.annotate({ "description": "Exact kind (`stream.started`) or `domain.*` prefix; same vocabulary as SSE `?kinds=`." }), "run": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Detached shell command: event JSON on stdin, `PF_EVENT_*` env." })), "timeout_s": Schema.optionalKey(Schema.Number.annotate({ "description": "Exec timeout in seconds (1–600, default 30); the process group is killed on expiry.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))), "webhook": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])) }).annotate({ "description": "One hook: `run` and/or `webhook` when `on` (+ `filter`) matches." })
export type SessionRow = { readonly "access_level"?: string | null, readonly "client": string, readonly "client_name"?: string | null, readonly "hdr": boolean, readonly "id"?: number, readonly "join": boolean, readonly "link"?: null | { readonly "abr_backoffs": number, readonly "abr_max_kbps": number, readonly "abr_min_kbps": number, readonly "anchor_p": number, readonly "egress_kbps": number, readonly "fec_max_pct": number, readonly "fec_min_pct": number, readonly "gaps": number, readonly "idr": number, readonly "intra_refresh": number, readonly "keyframe_req": number, readonly "loss_max_ppm": number, readonly "loss_mean_ppm": number, readonly "loss_windows": number, readonly "retargets": number, readonly "rfi": number, readonly "rfi_declined": number, readonly "secs": number, readonly "session_id": number, readonly "unrecovered": number, readonly "windows": number }, readonly "mode": string, readonly "muted": boolean, readonly "pads": ReadonlyArray<number>, readonly "plane": Plane, readonly "preferred_pad_slot"?: number, readonly "preset_name"?: string | null, readonly "shared_path_with"?: ReadonlyArray<number>, readonly "uptime_s": number }
export const SessionRow = Schema.Struct({ "access_level": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "`full` | `controller` | `view` | `custom`, live — not the pairing's stored level.\n`null` on the compat plane, which is ungoverned." })), "client": Schema.String.annotate({ "description": "Fingerprint prefix, or peer IP for an anonymous client." }), "client_name": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Display name (trust store, else the name the client sent). `null` if nameless." })), "hdr": Schema.Boolean, "id": Schema.optionalKey(Schema.Number.annotate({ "description": "Pass to `DELETE /session/{id}` and friends. `null` on the compat plane, which\nhas no per-session handle — stop it with the host-wide `DELETE /session`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))), "join": Schema.Boolean.annotate({ "description": "Sharing another session's display rather than owning one. Which session it joined\nis not reported yet — the two registries do not share ids (issue #1095)." }), "link": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "abr_backoffs": Schema.Number.annotate({ "description": "Client bitrate asks below the rate the encoder was running. The host cannot see which\nof loss, delay or decode drove them — only that the client's controller retreated.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "abr_max_kbps": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "abr_min_kbps": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "anchor_p": Schema.Number.annotate({ "description": "What the encoder answered an RFI with: a clean P against a surviving reference, or a\nwave that recodes the picture over ~0.5 s.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "egress_kbps": Schema.Number.annotate({ "description": "Sealed wire throughput over the window.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "fec_max_pct": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "fec_min_pct": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "gaps": Schema.Number.annotate({ "description": "Pipeline gaps this host announced: its own stalls, not the network's.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "idr": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "intra_refresh": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "keyframe_req": Schema.Number.annotate({ "description": "Client decode-recovery asks, and the IDRs actually forced (the cooldown coalesces).", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "loss_max_ppm": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "loss_mean_ppm": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "loss_windows": Schema.Number.annotate({ "description": "Of `windows`, those that reported any loss.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "retargets": Schema.Number.annotate({ "description": "Times the encoder's rate actually moved.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "rfi": Schema.Number.annotate({ "description": "Client reference-frame invalidation asks, and those the encoder refused (each refusal\ncosts an IDR).", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "rfi_declined": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "secs": Schema.Number.annotate({ "description": "Seconds this line covers. 60, except the last line of a session.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "session_id": Schema.Number.annotate({ "description": "The `/status` session id, so a line ties to a row.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "unrecovered": Schema.Number.annotate({ "description": "Windows that closed with frames parity could not repair.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "windows": Schema.Number.annotate({ "description": "Client loss reports that closed here. 0 = the client sent none, which is itself a fault.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "The last closed minute of link health: loss, the recovery frames it cost, and the FEC\nand bitrate bands. `null` in a session's first minute, and on the compat plane." })], { mode: "oneOf" })), "mode": Schema.String.annotate({ "description": "`WxH@Hz`." }), "muted": Schema.Boolean.annotate({ "description": "Audio is held back for this session alone (`PUT /session/{id}/audio`)." }), "pads": Schema.Array(Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))).annotate({ "description": "OS pad slots this session holds, lowest first. Slot `n` is player `n + 1` to a\nlocal co-op game. Empty while the session has no controller." }), "plane": Plane, "preferred_pad_slot": Schema.optionalKey(Schema.Number.annotate({ "description": "Player slot the operator picked for this session, 0-based (`PUT\n/session/{id}/player`). `null` = the slot is whichever comes free.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))), "preset_name": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Name of the settings preset the client dialled with. Absent for plain settings." })), "shared_path_with": Schema.optionalKey(Schema.Array(Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))).annotate({ "description": "Other live sessions from this client's address — one NAT or tunnel, so most likely one\nnetwork path. Their bitrates adapt independently. Absent when there are none." })), "uptime_s": Schema.Number.annotate({ "description": "Seconds since the stream started.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "One live session as the Dashboard lists it: who, where, since when, and the state\nthe per-session routes change." })
export type StreamRef = { readonly "app"?: string | null, readonly "client": string, readonly "fingerprint"?: string | null, readonly "hdr": boolean, readonly "mode": string, readonly "plane": Plane, readonly "preset"?: null | { readonly "id": string, readonly "name": string } }
export const StreamRef = Schema.Struct({ "app": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Store-qualified id on the native plane, app title on GameStream." })), "client": Schema.String, "fingerprint": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "The device's stable id — what a hook filter should key on, since a display name is\nneither unique nor fixed. Absent for an anonymous client." })), "hdr": Schema.Boolean, "mode": Schema.String.annotate({ "description": "`WxH@Hz`." }), "plane": Plane, "preset": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "id": Schema.String, "name": Schema.String }).annotate({ "description": "See [`ClientRef::preset`]." })], { mode: "oneOf" })) }).annotate({ "description": "Live video stream (what the stream marker file reflects)." })
export type PluginLogBatch = { readonly "entries": ReadonlyArray<PluginLogLine> }
export const PluginLogBatch = Schema.Struct({ "entries": Schema.Array(PluginLogLine) })
export type PluginSummary = { readonly "category"?: string | null, readonly "id": string, readonly "title": string, readonly "ui"?: null | PluginUiPublic, readonly "version"?: string | null }
export const PluginSummary = Schema.Struct({ "category": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "id": Schema.String, "title": Schema.String, "ui": Schema.optionalKey(Schema.Union([Schema.Null, PluginUiPublic], { mode: "oneOf" })), "version": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])) }).annotate({ "description": "Listing row. Never carries the secret — the browser reaches the UI only through the console proxy." })
export type HostInfo = { readonly "abi_version": number, readonly "app_version": string, readonly "codecs": ReadonlyArray<ApiCodec>, readonly "fingerprint"?: string | null, readonly "gamestream": boolean, readonly "gfe_version": string, readonly "hostname": string, readonly "local_ip": string, readonly "os": string, readonly "os_name": string, readonly "ports": PortMap, readonly "uniqueid": string, readonly "version": string }
export const HostInfo = Schema.Struct({ "abi_version": Schema.Number.annotate({ "description": "`punktfunk-core` C ABI version.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "app_version": Schema.String.annotate({ "description": "GameStream host version advertised to Moonlight clients." }), "codecs": Schema.Array(ApiCodec).annotate({ "description": "Codecs this host can encode (`host_wire_caps`, not the compile-time list)." }), "fingerprint": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Hex SHA-256 of this host's leaf certificate — what a client pins. Public by\nconstruction: every client reads it off the handshake. Carried here so a connect link\ncan name it, and a first connect over an untrusted path is verified rather than blind.\n`null` only if the identity could not be parsed." })), "gamestream": Schema.Boolean.annotate({ "description": "GameStream/Moonlight-compat planes are running (`--gamestream`). `false` is the default (native only)." }), "gfe_version": Schema.String.annotate({ "description": "GFE version advertised to Moonlight clients." }), "hostname": Schema.String, "local_ip": Schema.String.annotate({ "description": "Fresh LAN IP each request — do not cache. Cold-boot and network-move report\n`127.0.0.1` until a real address exists." }), "os": Schema.String.annotate({ "description": "OS chain, generic → specific, slash-separated (`windows` | `macos` |\n`linux[/<family>][/<id>]`). Walk most-specific-first; an unknown distro still matches its family." }), "os_name": Schema.String.annotate({ "description": "Human-readable OS name (os-release `PRETTY_NAME`; `\"Windows\"`/`\"macOS\"` elsewhere)." }), "ports": PortMap, "uniqueid": Schema.String.annotate({ "description": "Persisted host id; pairing matches on this." }), "version": Schema.String.annotate({ "description": "`punktfunk-host` crate version." }) }).annotate({ "description": "Host identity and capabilities. Static for the process except `local_ip`." })
export type DisplayLayoutRequest = { readonly "positions"?: { readonly [x: string]: Position } }
export const DisplayLayoutRequest = Schema.Struct({ "positions": Schema.optionalKey(Schema.Record(Schema.String, Position).annotate({ "description": "`{\"<identity_slot>\": {\"x\": …, \"y\": …}}` desktop top-left per slot." }).check(Schema.isPropertyNames(Schema.String))) }).annotate({ "description": "Manual layout: identity-slot id as string (same id `/display/state` reports) → desktop offset." })
export type Layout = { readonly "mode"?: LayoutMode, readonly "positions"?: { readonly [x: string]: Position } }
export const Layout = Schema.Struct({ "mode": Schema.optionalKey(LayoutMode), "positions": Schema.optionalKey(Schema.Record(Schema.String, Position).annotate({ "description": "Canonical decimal identity-slot ids (`\"1\"`..`\"15\"`) — the exact\nstring `arrange` looks up. [`DisplayPolicy::sanitized`] maps `\"01\"`\n→ `\"1\"` and drops non-ids; a key that never matches is a pin the\nconsole still shows while every session auto-rows past it." }).check(Schema.isPropertyNames(Schema.String))) })
export type HostCheck = { readonly "id": string, readonly "impact": string, readonly "params": { readonly [x: string]: string }, readonly "remedy"?: null | Remedy, readonly "severity": "info" | "warning" | "critical", readonly "since_unix"?: never, readonly "source": CheckSource, readonly "status": CheckStatus, readonly "summary": string }
export const HostCheck = Schema.Struct({ "id": Schema.String.annotate({ "description": "Console i18n key. See [`ids`]." }), "impact": Schema.String.annotate({ "description": "What breaks. Empty only on `ok`/`inapplicable`." }), "params": Schema.Record(Schema.String, Schema.String).annotate({ "description": "Interpolation for localized strings (`{user}`, `{group}`). Only the host can see these." }).check(Schema.isPropertyNames(Schema.String)), "remedy": Schema.optionalKey(Schema.Union([Schema.Null, Remedy], { mode: "oneOf" })), "severity": Schema.Literals(["info", "warning", "critical"]).annotate({ "description": "Meaningless on `ok`/`inapplicable`; carried so the wire shape never changes as status flips." }), "since_unix": Schema.optionalKey(Schema.Never), "source": CheckSource, "status": CheckStatus, "summary": Schema.String.annotate({ "description": "English fallback. Console localizes when it knows `id`." }) }).annotate({ "description": "One health verdict — the wire shape." })
export type ProviderRunningInput = { readonly "running"?: ReadonlyArray<RunningTitle> }
export const ProviderRunningInput = Schema.Struct({ "running": Schema.optionalKey(Schema.Array(RunningTitle).annotate({ "description": "Complete running set, not a delta: anything absent is reported as stopped." })) })
export type SessionSummary = { readonly "audio"?: null | { readonly "infilled": number, readonly "late": number, readonly "max_late_ms": number, readonly "reanchors": number, readonly "sent": number }, readonly "bit_depth": number, readonly "bitrate"?: null | { readonly "adaptive_steps": number, readonly "avg_kbps": number, readonly "max_kbps": number, readonly "min_kbps": number }, readonly "bitrate_kbps": number, readonly "bringup_ms": number, readonly "chroma": string, readonly "client": string, readonly "client_name"?: string | null, readonly "codec": string, readonly "duration_s": number, readonly "ended": SessionEndReason, readonly "frames_dropped"?: number, readonly "frames_sent"?: number, readonly "gyro"?: null | { readonly "samples": number, readonly "stalls": number }, readonly "hdr": boolean, readonly "id": number, readonly "input": InputCounts, readonly "join": boolean, readonly "mode": string, readonly "path_mtu"?: number, readonly "started_unix": number }
export const SessionSummary = Schema.Struct({ "audio": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "infilled": Schema.Number.annotate({ "description": "Frames synthesized over a capture hole. Wire continuity is not captured continuity.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "late": Schema.Number.annotate({ "description": "Departures a whole frame or more behind their slot.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "max_late_ms": Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "reanchors": Schema.Number.annotate({ "description": "Times the pacer fell far enough behind to forgive its debt and re-anchor.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "sent": Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "Absent when the session ran no audio plane (synthetic source, or the thread never started)." })], { mode: "oneOf" })), "bit_depth": Schema.Number.annotate({ "description": "8 or 10. Independent of `hdr` — 10-bit SDR is a mode.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "bitrate": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "adaptive_steps": Schema.Number.annotate({ "description": "Times the target moved after the opening rate: adaptive-bitrate decisions, plus a\nrebuild re-resolving what it actually encodes.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "avg_kbps": Schema.Number.annotate({ "description": "Mean of the targets the session ran at, NOT weighted by how long each held.\nA rate held for a second counts as much as one held for an hour.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "max_kbps": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "min_kbps": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "What the encoder's target did. Absent on a session that opened no encoder." })], { mode: "oneOf" })), "bitrate_kbps": Schema.Number.annotate({ "description": "The encoder's target when the session ended, kbps. `bitrate` has the span.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "bringup_ms": Schema.Number.annotate({ "description": "Hello → first video packet, ms. `0` if no packet ever left.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "chroma": Schema.String.annotate({ "description": "`4:2:0` or `4:4:4`." }), "client": Schema.String.annotate({ "description": "Cert-fingerprint prefix, or peer IP for an anonymous client." }), "client_name": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Display name (trust store, else the name the client sent). Absent if nameless." })), "codec": Schema.String.annotate({ "description": "`h264` | `hevc` | `av1` | `pyrowave`." }), "duration_s": Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ended": SessionEndReason, "frames_dropped": Schema.optionalKey(Schema.Number.annotate({ "description": "Frames the capturer's encode pool refused. Absent on a capturer that does not\ncount them (everything but the Windows IDD push path).", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))), "frames_sent": Schema.optionalKey(Schema.Number.annotate({ "description": "Access units the send thread put on the wire. Absent when the video loop\nnever reached its tail — the `host_error` case.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))), "gyro": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "samples": Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "stalls": Schema.Number.annotate({ "description": "Gaps of 500 ms or more — the feed stopping, not jitter.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "Absent when no pad sent motion — most sessions." })], { mode: "oneOf" })), "hdr": Schema.Boolean, "id": Schema.Number.annotate({ "description": "Same id [`SessionRef`] and the per-session routes carry.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "input": InputCounts, "join": Schema.Boolean.annotate({ "description": "Shared another session's display at ITS mode instead of owning one." }), "mode": Schema.String.annotate({ "description": "`WxH@Hz` as last delivered; a mid-session mode switch moves it." }), "path_mtu": Schema.optionalKey(Schema.Number.annotate({ "description": "Path MTU the QUIC stack settled on, bytes. Absent as `frames_sent` is.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))), "started_unix": Schema.Number.annotate({ "description": "Host wall clock at stream start, unix seconds.", "format": "int64" }).check(Schema.isInt()) }).annotate({ "description": "Everything the host knows about one finished session.\n\nThe `session.ended` payload AND the body of `GET /api/v1/session/last`: one struct,\nso a screenshot of the console card is the API answer a bug report would have carried.\nA number the host does not accumulate per session is absent rather than zero." })
export type SettingState = { readonly "advanced": boolean, readonly "apply": SettingApply, readonly "default": Schema.Json, readonly "docs": string, readonly "env": string, readonly "group": SettingGroup, readonly "id": string, readonly "kind": SettingKind, readonly "max"?: never, readonly "max_len"?: never, readonly "min"?: never, readonly "options"?: ReadonlyArray<string>, readonly "origin"?: string | null, readonly "restart_pending": boolean, readonly "source": SettingSource, readonly "stored"?: Schema.Json, readonly "title": string, readonly "unit"?: string | null, readonly "value": Schema.Json }
export const SettingState = Schema.Struct({ "advanced": Schema.Boolean, "apply": SettingApply, "default": Schema.Json, "docs": Schema.String.annotate({ "description": "docs-site page slug under `/docs/`." }), "env": Schema.String.annotate({ "description": "The name to set in `host.env` to pin this setting." }), "group": SettingGroup, "id": Schema.String.annotate({ "description": "Store key; the console's copy key is `setting_<id>`." }), "kind": SettingKind, "max": Schema.optionalKey(Schema.Never), "max_len": Schema.optionalKey(Schema.Never), "min": Schema.optionalKey(Schema.Never), "options": Schema.optionalKey(Schema.Union([Schema.Array(Schema.String).annotate({ "description": "`enum` only, canonical spellings in display order." })])), "origin": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "The env var or CLI flag that set `value` (`source` `env` or `flag`)." })), "restart_pending": Schema.Boolean.annotate({ "description": "Changed since the host started, and applies only after a restart." }), "source": SettingSource, "stored": Schema.optionalKey(Schema.Json.annotate({ "description": "The console's value, even while an env var or flag overrides it." })), "title": Schema.String.annotate({ "description": "English label, for a setting the console has no copy for." }), "unit": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "`int` and `decimal` only, e.g. `fps`." })), "value": Schema.Json.annotate({ "description": "The value in force." }) }).annotate({ "description": "One setting as the console renders it." })
export type CatalogResponse = { readonly "busy": boolean, readonly "host": HostFacts, readonly "plugins": ReadonlyArray<CatalogEntry>, readonly "sources": ReadonlyArray<SourceView> }
export const CatalogResponse = Schema.Struct({ "busy": Schema.Boolean, "host": HostFacts, "plugins": Schema.Array(CatalogEntry), "sources": Schema.Array(SourceView) })
export type StatsSample = { readonly "bitrate_kbps": number, readonly "fec_recovered"?: number, readonly "fec_us"?: number, readonly "fps": number, readonly "frames_dropped"?: number, readonly "host_p50_us"?: number, readonly "host_p99_us"?: number, readonly "mbps": number, readonly "packets_dropped"?: number, readonly "repeat_fps": number, readonly "rtt_us"?: number, readonly "seal_us"?: number, readonly "send_dropped"?: number, readonly "session_id": number, readonly "sock_us"?: number, readonly "stages": ReadonlyArray<StageTiming>, readonly "t_ms": number }
export const StatsSample = Schema.Struct({ "bitrate_kbps": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "fec_recovered": Schema.optionalKey(Schema.Number.annotate({ "description": "FEC shards the receiver recovered. Only a client measures it.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))), "fec_us": Schema.optionalKey(Schema.Number.annotate({ "description": "Sealing one frame, mean µs over the window: FEC parity, AES-GCM, the socket sends.\nNative only; timed while a capture runs.", "format": "float" }).check(Schema.isFinite())), "fps": Schema.Number.annotate({ "description": "Genuine new frames/s from the source (not including repeats).", "format": "float" }).check(Schema.isFinite()), "frames_dropped": Schema.optionalKey(Schema.Number.annotate({ "description": "Counters are deltas for this window. `None` = this path cannot see it, never a zero it\ndid not measure. Frames the host dropped: the driver's pool, or GameStream's queue.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))), "host_p50_us": Schema.optionalKey(Schema.Number.annotate({ "description": "Capture → fully sent, p50/p99 µs: the span a client's `host` term reports.", "format": "float" }).check(Schema.isFinite())), "host_p99_us": Schema.optionalKey(Schema.Number.annotate({ "format": "float" }).check(Schema.isFinite())), "mbps": Schema.Number.annotate({ "description": "Attempted sealed wire Mb/s at seal time (AU + shard framing + FEC, including\ndatagram-aligned zero-pad). Not goodput; socket send drops do not reduce it.", "format": "float" }).check(Schema.isFinite()), "packets_dropped": Schema.optionalKey(Schema.Number.annotate({ "description": "Receiver-side loss. Only a client measures it; recordings older than this field hold 0.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))), "repeat_fps": Schema.Number.annotate({ "description": "Re-encoded holds/s — source starvation, not new frames.", "format": "float" }).check(Schema.isFinite()), "rtt_us": Schema.optionalKey(Schema.Number.annotate({ "description": "Smoothed QUIC round trip to the client, µs.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))), "seal_us": Schema.optionalKey(Schema.Number.annotate({ "format": "float" }).check(Schema.isFinite())), "send_dropped": Schema.optionalKey(Schema.Number.annotate({ "description": "Host send-buffer overflow / EAGAIN.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))), "session_id": Schema.Number.annotate({ "description": "Distinguishes concurrent sessions (usually constant for one loop).", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "sock_us": Schema.optionalKey(Schema.Number.annotate({ "format": "float" }).check(Schema.isFinite())), "stages": Schema.Array(StageTiming), "t_ms": Schema.Number.annotate({ "description": "Milliseconds since capture start (monotonic; stamped by [`StatsRecorder::push_sample`]).", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "One aggregated sample (~2 s native, ~1 s GameStream)." })
export type Job = { readonly "error"?: string | null, readonly "finished_at"?: never, readonly "id": string, readonly "kind": string, readonly "log": ReadonlyArray<string>, readonly "phase": string, readonly "started_at": number, readonly "state": State, readonly "target": string }
export const Job = Schema.Struct({ "error": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "finished_at": Schema.optionalKey(Schema.Never), "id": Schema.String, "kind": Schema.String, "log": Schema.Array(Schema.String), "phase": Schema.String, "started_at": Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "state": State, "target": Schema.String }).annotate({ "description": "Snake_case matches the management API; index/sources/manifest files use npm camelCase." })
export type ClientOverlay = { readonly "capture_monitor"?: string | null, readonly "game_session"?: null | GameSession, readonly "identity"?: null | Identity, readonly "keep_alive"?: null | KeepAlive, readonly "max_mode"?: string | null, readonly "mode_conflict"?: null | ModeConflict, readonly "scale"?: never, readonly "topology"?: null | Topology }
export const ClientOverlay = Schema.Struct({ "capture_monitor": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Mirror this connector for this device only. Absent follows the host,\nwhich is also the only way back to a virtual screen — a device cannot\nopt OUT of a host-wide pin. Nobody has asked to; the reverse direction\nis what the field exists for." })), "game_session": Schema.optionalKey(Schema.Union([Schema.Null, GameSession], { mode: "oneOf" })), "identity": Schema.optionalKey(Schema.Union([Schema.Null, Identity], { mode: "oneOf" })), "keep_alive": Schema.optionalKey(Schema.Union([Schema.Null, KeepAlive], { mode: "oneOf" })), "max_mode": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Largest mode this device is granted, `WIDTHxHEIGHT@HZ`. A phone asking for 4K120\non a weak host degrades every other session; the operator caps it once and the\nclient is told the smaller mode rather than silently given one." })), "mode_conflict": Schema.optionalKey(Schema.Union([Schema.Null, ModeConflict], { mode: "oneOf" })), "scale": Schema.optionalKey(Schema.Never), "topology": Schema.optionalKey(Schema.Union([Schema.Null, Topology], { mode: "oneOf" })) }).annotate({ "description": "One paired device's deviations from the host policy\n(`design/web-console-overhaul.md` §6.1).\n\nEvery field is optional and absent means **follow the host**. That is the\nwhole point: a copied policy would silently stop following host changes,\nwhile an overlay only pins what the operator actually chose for this\ndevice. The TV wants take-over and keep-forever; the tablet wants its own\nscreen and no linger — one host policy cannot serve both.\n\n`max_displays` and `layout` are deliberately absent: they are properties of\nthe host's desktop, not of a device connecting to it." })
export type UpdateStatus = { readonly "apply": string, readonly "available": boolean, readonly "channel": string, readonly "channel_hint": string, readonly "check_disabled": boolean, readonly "current_version": string, readonly "install_kind": string, readonly "job"?: null | UpdateJobInfo, readonly "last_checked_unix"?: never, readonly "last_error"?: string | null, readonly "last_result"?: null | UpdateResultInfo, readonly "manifest"?: null | UpdateManifestInfo, readonly "not_published": boolean, readonly "opt_in_hint"?: string | null }
export const UpdateStatus = Schema.Struct({ "apply": Schema.String.annotate({ "description": "`notify` (show the command) | `full` (one-click) | `staged` (apply + reboot)." }), "available": Schema.Boolean.annotate({ "description": "Newer than `current_version` on this channel. Unparseable pairs never flag." }), "channel": Schema.String.annotate({ "description": "`stable` | `canary`." }), "channel_hint": Schema.String.annotate({ "description": "Copy-paste update command for this install kind." }), "check_disabled": Schema.Boolean.annotate({ "description": "`PUNKTFUNK_UPDATE_CHECK=0`." }), "current_version": Schema.String, "install_kind": Schema.String.annotate({ "description": "`windows-installer` | `sysext` | `rpm-ostree` | `apt` | `dnf` | `pacman` |\n`steamos-source` | `nix` | `source`." }), "job": Schema.optionalKey(Schema.Union([Schema.Null, UpdateJobInfo], { mode: "oneOf" })), "last_checked_unix": Schema.optionalKey(Schema.Never), "last_error": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Last check failure, verbatim." })), "last_result": Schema.optionalKey(Schema.Union([Schema.Null, UpdateResultInfo], { mode: "oneOf" })), "manifest": Schema.optionalKey(Schema.Union([Schema.Null, UpdateManifestInfo], { mode: "oneOf" })), "not_published": Schema.Boolean.annotate({ "description": "Feed 404 for this channel: nothing published yet, not a check failure.\nMutually exclusive with `last_error`. Never set once a manifest has been\nseen — a feed that then 404s is an error." }), "opt_in_hint": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "One-click apply is possible but not opted in — command to run\n(Linux: join `punktfunk-update`)." })) })
export type CustomEntry = { readonly "description"?: string | null, readonly "developer"?: string | null, readonly "genres"?: ReadonlyArray<string>, readonly "platform"?: string | null, readonly "players"?: never, readonly "publisher"?: string | null, readonly "region"?: string | null, readonly "release_year"?: never, readonly "tags"?: ReadonlyArray<string>, readonly "art"?: Artwork, readonly "audio"?: null | { readonly "sessions"?: AudioSessions }, readonly "detect"?: { readonly "env_marker"?: null | EnvMarker, readonly "exe"?: string | null, readonly "install_dir"?: string | null, readonly "process_name"?: string | null, readonly "steam_appid"?: never }, readonly "external_id"?: string | null, readonly "icon"?: string | null, readonly "id": string, readonly "ids"?: { readonly [x: string]: string }, readonly "launch"?: null | LaunchSpec, readonly "on_window"?: { readonly "focus"?: boolean | null, readonly "fullscreen"?: boolean | null, readonly "move_to_stream_output"?: boolean | null, readonly "workspace"?: null | WorkspacePlacement }, readonly "prep"?: ReadonlyArray<PrepCmd>, readonly "provider"?: string | null, readonly "role"?: GameRole, readonly "store"?: string | null, readonly "title": string }
export const CustomEntry = Schema.Struct({ "description": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "developer": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "genres": Schema.optionalKey(Schema.Array(Schema.String)), "platform": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "`\"PS2\"`, `\"Xbox 360\"`, `\"SNES\"`, … Installed-store scanners stamp `\"PC\"`;\n`GET /library?platform=` filters on it (case-insensitive)." })), "players": Schema.optionalKey(Schema.Never), "publisher": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "region": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "release_year": Schema.optionalKey(Schema.Never), "tags": Schema.optionalKey(Schema.Array(Schema.String)), "art": Schema.optionalKey(Artwork), "audio": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "sessions": Schema.optionalKey(AudioSessions) }).annotate({ "description": "Which sessions hear this title. Absent = every session." })], { mode: "oneOf" })), "detect": Schema.optionalKey(Schema.Struct({ "env_marker": Schema.optionalKey(Schema.Union([Schema.Null, EnvMarker], { mode: "oneOf" })), "exe": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "install_dir": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "process_name": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Weakest signal — see [`DetectSpec::process_name`]." })), "steam_appid": Schema.optionalKey(Schema.Never) }).annotate({ "description": "How to find this title after a command that hands off and exits. Without it the\nhost tracks only the child it spawned." })), "external_id": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Provider's reconcile key. Present iff `provider` is, so the host `id` can stay put." })), "icon": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Brand token (`steam`, `heroic`) a client draws. Never bytes, never a URL. See [`GameEntry::icon`]." })), "id": Schema.String.annotate({ "description": "Host-assigned row id; the `{id}` in the CRUD path. Not the surfaced library id." }), "ids": Schema.optionalKey(Schema.Record(Schema.String, Schema.String).annotate({ "description": "Catalog ids a metadata source matches on. Set only by provider reconcile." }).check(Schema.isPropertyNames(Schema.String))), "launch": Schema.optionalKey(Schema.Union([Schema.Null, LaunchSpec], { mode: "oneOf" })), "on_window": Schema.optionalKey(Schema.Struct({ "focus": Schema.optionalKey(Schema.Union([Schema.Boolean, Schema.Null]).annotate({ "description": "Raise it. Default on: the player launched it, so it is what they want\nin front — a launcher that steals focus back leaves them on the desk." })), "fullscreen": Schema.optionalKey(Schema.Union([Schema.Boolean, Schema.Null]).annotate({ "description": "Make it full-screen. Default off: most games set their own mode, and\nforcing it fights a title that wanted a window." })), "move_to_stream_output": Schema.optionalKey(Schema.Union([Schema.Boolean, Schema.Null]).annotate({ "description": "Move it onto the streamed head if it opened elsewhere. Default on: a\ngame the player cannot see is the whole failure this stage exists for." })), "workspace": Schema.optionalKey(Schema.Union([Schema.Null, WorkspacePlacement], { mode: "oneOf" })) }).annotate({ "description": "Which workspace this title opens on ([`crate::library::OnWindow`]).\nAbsent follows the host's display policy." })), "prep": Schema.optionalKey(Schema.Array(PrepCmd).annotate({ "description": "Each `do` runs before launch; each `undo` at session end in reverse ([`crate::hooks::run_prep`])." })), "provider": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Set only by provider reconcile. `None` is a manual row: reconcile never touches it, and\nmanual CRUD refuses provider-owned rows." })), "role": Schema.optionalKey(GameRole), "store": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Store this row was claimed under. `None` surfaces as `custom`. Stamped on the\nrow so id and badge stay correct while [`Catalog::claims`] is being rewritten." })), "title": Schema.String }).annotate({ "description": "Optional display metadata, `#[serde(flatten)]`-ed into [`GameEntry`].\n\nValues are free-form strings, not enums — emulation sources (RomM, EmuDeck,\nPlaynite) each have their own vocabulary and the host does not normalize it." })
export type CustomInput = { readonly "description"?: string | null, readonly "developer"?: string | null, readonly "genres"?: ReadonlyArray<string>, readonly "platform"?: string | null, readonly "players"?: never, readonly "publisher"?: string | null, readonly "region"?: string | null, readonly "release_year"?: never, readonly "tags"?: ReadonlyArray<string>, readonly "art"?: Artwork, readonly "audio"?: null | { readonly "sessions"?: AudioSessions }, readonly "detect"?: null | { readonly "env_marker"?: null | EnvMarker, readonly "exe"?: string | null, readonly "install_dir"?: string | null, readonly "process_name"?: string | null, readonly "steam_appid"?: never }, readonly "icon"?: string | null, readonly "launch"?: null | LaunchSpec, readonly "on_window"?: null | { readonly "focus"?: boolean | null, readonly "fullscreen"?: boolean | null, readonly "move_to_stream_output"?: boolean | null, readonly "workspace"?: null | WorkspacePlacement }, readonly "prep"?: ReadonlyArray<{ readonly "do": string, readonly "undo"?: string | null }>, readonly "role"?: "game" | "launcher", readonly "title": string }
export const CustomInput = Schema.Struct({ "description": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "developer": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "genres": Schema.optionalKey(Schema.Array(Schema.String)), "platform": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "`\"PS2\"`, `\"Xbox 360\"`, `\"SNES\"`, … Installed-store scanners stamp `\"PC\"`;\n`GET /library?platform=` filters on it (case-insensitive)." })), "players": Schema.optionalKey(Schema.Never), "publisher": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "region": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "release_year": Schema.optionalKey(Schema.Never), "tags": Schema.optionalKey(Schema.Array(Schema.String)), "art": Schema.optionalKey(Artwork), "audio": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "sessions": Schema.optionalKey(AudioSessions) }).annotate({ "description": "Absent on an update keeps the stored policy; `{\"sessions\":\"all\"}` clears it." })], { mode: "oneOf" })), "detect": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "env_marker": Schema.optionalKey(Schema.Union([Schema.Null, EnvMarker], { mode: "oneOf" })), "exe": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "install_dir": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "process_name": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Weakest signal — see [`DetectSpec::process_name`]." })), "steam_appid": Schema.optionalKey(Schema.Never) }).annotate({ "description": "Absent on an update keeps the stored hint, as with `prep`." })], { mode: "oneOf" })), "icon": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Brand token. A hand-added \"Steam\" tile can look like one. See [`GameEntry::icon`]." })), "launch": Schema.optionalKey(Schema.Union([Schema.Null, LaunchSpec], { mode: "oneOf" })), "on_window": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "focus": Schema.optionalKey(Schema.Union([Schema.Boolean, Schema.Null]).annotate({ "description": "Raise it. Default on: the player launched it, so it is what they want\nin front — a launcher that steals focus back leaves them on the desk." })), "fullscreen": Schema.optionalKey(Schema.Union([Schema.Boolean, Schema.Null]).annotate({ "description": "Make it full-screen. Default off: most games set their own mode, and\nforcing it fights a title that wanted a window." })), "move_to_stream_output": Schema.optionalKey(Schema.Union([Schema.Boolean, Schema.Null]).annotate({ "description": "Move it onto the streamed head if it opened elsewhere. Default on: a\ngame the player cannot see is the whole failure this stage exists for." })), "workspace": Schema.optionalKey(Schema.Union([Schema.Null, WorkspacePlacement], { mode: "oneOf" })) }).annotate({ "description": "Absent on an update keeps the stored placement, as with `prep`." })], { mode: "oneOf" })), "prep": Schema.optionalKey(Schema.Union([Schema.Array(Schema.Struct({ "do": Schema.String.annotate({ "description": "Command run before launch. Same recipe and ownership checks as hook `run`; stdin is `{}`." }), "undo": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "After session end. Skipped when its `do` failed (it never took effect)." })) }).annotate({ "description": "Per-app prep (Sunshine `prep-cmd` parity): `do` runs synchronously before launch;\n`undo` runs at session end, reverse order, best-effort, including panic-unwind ([`PrepGuard`])." })).annotate({ "description": "Run as the host user; operator-privileged. See [`privileged_field`]. Absent on an\nupdate keeps the stored list: a writer that never read it must not clear it." })])), "role": Schema.optionalKey(Schema.Literals(["game", "launcher"]).annotate({ "description": "A hand-added launcher tile is legal without installing the matching plugin." })), "title": Schema.String }).annotate({ "description": "Flattened [`GameMeta`]. Replaced wholesale on update — an edit must send every field it wants kept." })
export type OperatorGameEntry = { readonly "description"?: string | null, readonly "developer"?: string | null, readonly "genres"?: ReadonlyArray<string>, readonly "platform"?: string | null, readonly "players"?: never, readonly "publisher"?: string | null, readonly "region"?: string | null, readonly "release_year"?: never, readonly "tags"?: ReadonlyArray<string>, readonly "art": Artwork, readonly "filled"?: { readonly [x: string]: string }, readonly "icon"?: string | null, readonly "id": string, readonly "ids"?: { readonly [x: string]: string }, readonly "launch"?: null | LaunchSpec, readonly "on_window"?: { readonly "focus"?: boolean | null, readonly "fullscreen"?: boolean | null, readonly "move_to_stream_output"?: boolean | null, readonly "workspace"?: null | WorkspacePlacement }, readonly "provider"?: string | null, readonly "role"?: GameRole, readonly "stats"?: null | { readonly "last_played_unix_ms": number, readonly "last_run_ms": number, readonly "launch_count": number, readonly "play_time_ms": number }, readonly "store": string, readonly "title": string, readonly "hidden"?: boolean }
export const OperatorGameEntry = Schema.Struct({ "description": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "developer": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "genres": Schema.optionalKey(Schema.Array(Schema.String)), "platform": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "`\"PS2\"`, `\"Xbox 360\"`, `\"SNES\"`, … Installed-store scanners stamp `\"PC\"`;\n`GET /library?platform=` filters on it (case-insensitive)." })), "players": Schema.optionalKey(Schema.Never), "publisher": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "region": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "release_year": Schema.optionalKey(Schema.Never), "tags": Schema.optionalKey(Schema.Array(Schema.String)), "art": Artwork, "filled": Schema.optionalKey(Schema.Record(Schema.String, Schema.String).annotate({ "description": "Where each borrowed value came from: art slot or meta field → source id, or `pick`\n([`PICK`]). Values the entry carried itself are absent. Not sent to paired clients." }).check(Schema.isPropertyNames(Schema.String))), "icon": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Brand-mark token (`steam`, `heroic`) — never bytes or a URL. See [`is_icon_token`].\n\nThe art proxy serves raster only ([`art::local_art_bytes`] refuses SVG as\nscript-capable XML), so the mark stays a name the client already ships." })), "id": Schema.String.annotate({ "description": "Stable, store-qualified id: `steam:<appid>` or `custom:<id>`." }), "ids": Schema.optionalKey(Schema.Record(Schema.String, Schema.String).annotate({ "description": "Catalog ids a metadata source matches on (`steam`, `gog`, `libretro`, `sgdb` → value),\nset by the plugin that lists the entry. Not sent to paired clients." }).check(Schema.isPropertyNames(Schema.String))), "launch": Schema.optionalKey(Schema.Union([Schema.Null, LaunchSpec], { mode: "oneOf" })), "on_window": Schema.optionalKey(Schema.Struct({ "focus": Schema.optionalKey(Schema.Union([Schema.Boolean, Schema.Null]).annotate({ "description": "Raise it. Default on: the player launched it, so it is what they want\nin front — a launcher that steals focus back leaves them on the desk." })), "fullscreen": Schema.optionalKey(Schema.Union([Schema.Boolean, Schema.Null]).annotate({ "description": "Make it full-screen. Default off: most games set their own mode, and\nforcing it fights a title that wanted a window." })), "move_to_stream_output": Schema.optionalKey(Schema.Union([Schema.Boolean, Schema.Null]).annotate({ "description": "Move it onto the streamed head if it opened elsewhere. Default on: a\ngame the player cannot see is the whole failure this stage exists for." })), "workspace": Schema.optionalKey(Schema.Union([Schema.Null, WorkspacePlacement], { mode: "oneOf" })) }).annotate({ "description": "Which workspace this title's windows open on, where the compositor can\nplace them ([`OnWindow`])." })), "provider": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Plugin that owns this entry. `None` only for operator-typed custom entries.\n`GET /library?provider=` filters on it." })), "role": Schema.optionalKey(GameRole), "stats": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "last_played_unix_ms": Schema.Number.annotate({ "description": "Unix ms of the most recent launch. Stamped at launch, so a game just started sorts first.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "last_run_ms": Schema.Number.annotate({ "description": "The run that started at `last_played_unix_ms`. Still growing while it runs.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "launch_count": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "play_time_ms": Schema.Number.annotate({ "description": "Every run added up.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "Play stats, once this host has launched the title (`stats.rs`). Joined at read\ntime from `library-stats.json`, never stored on the entry." })], { mode: "oneOf" })), "store": Schema.String.annotate({ "description": "Which store surfaced it: `\"steam\"` or `\"custom\"`." }), "title": Schema.String, "hidden": Schema.optionalKey(Schema.Boolean.annotate({ "description": "Set by [`set_entry_hidden`]. Omitted when false so the shape only grows for hidden titles." })) }).annotate({ "description": "Optional display metadata, `#[serde(flatten)]`-ed into [`GameEntry`].\n\nValues are free-form strings, not enums — emulation sources (RomM, EmuDeck,\nPlaynite) each have their own vocabulary and the host does not normalize it." })
export type ProviderEntryInput = { readonly "description"?: string | null, readonly "developer"?: string | null, readonly "genres"?: ReadonlyArray<string>, readonly "platform"?: string | null, readonly "players"?: never, readonly "publisher"?: string | null, readonly "region"?: string | null, readonly "release_year"?: never, readonly "tags"?: ReadonlyArray<string>, readonly "art"?: Artwork, readonly "audio"?: null | AudioPolicy, readonly "detect"?: { readonly "env_marker"?: null | EnvMarker, readonly "exe"?: string | null, readonly "install_dir"?: string | null, readonly "process_name"?: string | null, readonly "steam_appid"?: never }, readonly "external_id": string, readonly "icon"?: string | null, readonly "ids"?: { readonly [x: string]: string }, readonly "launch"?: null | LaunchSpec, readonly "on_window"?: { readonly "focus"?: boolean | null, readonly "fullscreen"?: boolean | null, readonly "move_to_stream_output"?: boolean | null, readonly "workspace"?: null | WorkspacePlacement }, readonly "prep"?: ReadonlyArray<PrepCmd>, readonly "role"?: "game" | "launcher", readonly "title": string }
export const ProviderEntryInput = Schema.Struct({ "description": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "developer": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "genres": Schema.optionalKey(Schema.Array(Schema.String)), "platform": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "`\"PS2\"`, `\"Xbox 360\"`, `\"SNES\"`, … Installed-store scanners stamp `\"PC\"`;\n`GET /library?platform=` filters on it (case-insensitive)." })), "players": Schema.optionalKey(Schema.Never), "publisher": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "region": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "release_year": Schema.optionalKey(Schema.Never), "tags": Schema.optionalKey(Schema.Array(Schema.String)), "art": Schema.optionalKey(Artwork), "audio": Schema.optionalKey(Schema.Union([Schema.Null, AudioPolicy], { mode: "oneOf" })), "detect": Schema.optionalKey(Schema.Struct({ "env_marker": Schema.optionalKey(Schema.Union([Schema.Null, EnvMarker], { mode: "oneOf" })), "exe": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "install_dir": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])), "process_name": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Weakest signal — see [`DetectSpec::process_name`]." })), "steam_appid": Schema.optionalKey(Schema.Never) }).annotate({ "description": "Install-dir / process hint. Needed when launch goes through the provider's own client." })), "external_id": Schema.String, "icon": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Brand token a plugin sets on its `launchers(cfg)` tiles. See [`GameEntry::icon`]." })), "ids": Schema.optionalKey(Schema.Record(Schema.String, Schema.String).annotate({ "description": "Catalog ids a metadata source matches on: `steam` → appid, `libretro` →\n`<system>/<No-Intro name>`. Keys `[a-z0-9_]{1,16}`, at most eight; a bad pair is dropped." }).check(Schema.isPropertyNames(Schema.String))), "launch": Schema.optionalKey(Schema.Union([Schema.Null, LaunchSpec], { mode: "oneOf" })), "on_window": Schema.optionalKey(Schema.Struct({ "focus": Schema.optionalKey(Schema.Union([Schema.Boolean, Schema.Null]).annotate({ "description": "Raise it. Default on: the player launched it, so it is what they want\nin front — a launcher that steals focus back leaves them on the desk." })), "fullscreen": Schema.optionalKey(Schema.Union([Schema.Boolean, Schema.Null]).annotate({ "description": "Make it full-screen. Default off: most games set their own mode, and\nforcing it fights a title that wanted a window." })), "move_to_stream_output": Schema.optionalKey(Schema.Union([Schema.Boolean, Schema.Null]).annotate({ "description": "Move it onto the streamed head if it opened elsewhere. Default on: a\ngame the player cannot see is the whole failure this stage exists for." })), "workspace": Schema.optionalKey(Schema.Union([Schema.Null, WorkspacePlacement], { mode: "oneOf" })) }).annotate({ "description": "Which workspace the title opens on; absent follows the host's display policy." })), "prep": Schema.optionalKey(Schema.Array(PrepCmd).annotate({ "description": "Run as the host user; operator-privileged. See [`privileged_field`]." })), "role": Schema.optionalKey(Schema.Literals(["game", "launcher"]).annotate({ "description": "Plugins emit `launchers(cfg)` tiles with `role: \"launcher\"`." })), "title": Schema.String }).annotate({ "description": "Optional display metadata, `#[serde(flatten)]`-ed into [`GameEntry`].\n\nValues are free-form strings, not enums — emulation sources (RomM, EmuDeck,\nPlaynite) each have their own vocabulary and the host does not normalize it." })
export type MetadataInput = { readonly "entries"?: ReadonlyArray<MetadataEntryInput>, readonly "matching"?: Matching }
export const MetadataInput = Schema.Struct({ "entries": Schema.optionalKey(Schema.Array(MetadataEntryInput)), "matching": Schema.optionalKey(Matching) }).annotate({ "description": "`PUT /library/metadata/{source}`: the source's whole result." })
export type SessionSettingsState = { readonly "configured": boolean, readonly "enforced": ReadonlyArray<string>, readonly "settings": SessionSettings }
export const SessionSettingsState = Schema.Struct({ "configured": Schema.Boolean.annotate({ "description": "`false` means `settings` are the built-in defaults." }), "enforced": Schema.Array(Schema.String).annotate({ "description": "Axes this build enforces. Empty with no launch path (macOS) so the console\ndoes not offer a no-op switch." }), "settings": SessionSettings })
export type HooksConfig = { readonly "hooks"?: ReadonlyArray<HookEntry> }
export const HooksConfig = Schema.Struct({ "hooks": Schema.optionalKey(Schema.Array(HookEntry)) }).annotate({ "description": "Operator hook config: `hooks.json` and the `/api/v1/hooks` body." })
export type RuntimeStatus = { readonly "active_sessions": number, readonly "audio"?: null | { readonly "last_resort": boolean, readonly "loopback"?: string | null, readonly "mic"?: string | null, readonly "mic_withheld": boolean, readonly "narrowing"?: string | null, readonly "readiness": string }, readonly "audio_streaming": boolean, readonly "display"?: null | { readonly "driver_protocol"?: never, readonly "host_protocol": number, readonly "last_transaction"?: null | { readonly "outcome": string, readonly "reason": string, readonly "took_ms": number }, readonly "pnp_leases": number, readonly "topology_generation": number }, readonly "games": ReadonlyArray<ActiveGame>, readonly "native_paired_clients": number, readonly "paired_clients": number, readonly "pin_pending": boolean, readonly "session"?: null | { readonly "capture"?: null | { readonly "backend_opened"?: string | null, readonly "class": string, readonly "cooldown_remaining_ms"?: never, readonly "current_stage"?: string | null, readonly "detached": number, readonly "dropped_total": number, readonly "encoder_state"?: string | null, readonly "episodes_suppressed": number, readonly "evidence"?: string | null, readonly "last_episode"?: null | { readonly "consecutive_failures": number, readonly "cooldown_ms": number, readonly "recovered": boolean, readonly "stages": ReadonlyArray<CaptureStage>, readonly "stall_class": string, readonly "took_ms": number }, readonly "late_frames": boolean, readonly "present_to_arrival_ms"?: never, readonly "published_total": number, readonly "source_gap_ms": number, readonly "stall_class"?: string | null }, readonly "fps": number, readonly "height": number, readonly "width": number }, readonly "session_id"?: number, readonly "sessions": ReadonlyArray<SessionRow>, readonly "stream"?: null | { readonly "bitrate_kbps": number, readonly "codec": ApiCodec, readonly "fps": number, readonly "height": number, readonly "last_resize_ms"?: never, readonly "min_fec": number, readonly "packet_size": number, readonly "time_to_first_frame_ms"?: never, readonly "width": number }, readonly "video_streaming": boolean }
export const RuntimeStatus = Schema.Struct({ "active_sessions": Schema.Number.annotate({ "description": "Live sessions on both planes. Native admits concurrent sessions so this can exceed 1;\n`session`/`stream` are one representative and `sessions` is the list.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "audio": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "last_resort": Schema.Boolean.annotate({ "description": "Loopback is the degraded last resort; desktop audio may be silent until endpoints change." }), "loopback": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Desktop-audio loopback friendly name; absent = unavailable." })), "mic": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Virtual-mic write-target friendly name; absent = unavailable." })), "mic_withheld": Schema.Boolean.annotate({ "description": "Mic withheld so game audio keeps the only working sink." }), "narrowing": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Why the chosen loopback endpoint NARROWS the desktop mix (rate/channels), when it does." })), "readiness": Schema.String.annotate({ "description": "`full` | `audio_only` | `mic_only` | `none`." }) }).annotate({ "description": "Windows audio-wiring verdict; absent off-Windows and before the first pass. Present while idle." })], { mode: "oneOf" })), "audio_streaming": Schema.Boolean, "display": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "driver_protocol": Schema.optionalKey(Schema.Never), "host_protocol": Schema.Number.annotate({ "description": "Protocol this host drives; a driver answering less fails every session.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "last_transaction": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "outcome": Schema.String.annotate({ "description": "`changed` / `unchanged` / `unknown`." }), "reason": Schema.String.annotate({ "description": "`acquire-isolate` / `exclusive-reassert` / …" }), "took_ms": Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "The most recent topology transaction, if any." })], { mode: "oneOf" })), "pnp_leases": Schema.Number.annotate({ "description": "Monitor devnodes disabled for a stream and not yet re-enabled (a leftover here after a\ncrash is what the next host start replays).", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "topology_generation": Schema.Number.annotate({ "description": "Count of topology transactions that observed a change since the host started.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "Windows display state: topology transactions and outstanding device leases. Absent off-Windows." })], { mode: "oneOf" })), "games": Schema.Array(ActiveGame).annotate({ "description": "Launched titles: live sessions plus `state: \"grace\"` reconnect-window rows. Empty for a desktop-only stream." }), "native_paired_clients": Schema.Number.annotate({ "description": "Native-plane pairings (separate store).", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "paired_clients": Schema.Number.annotate({ "description": "GameStream paired-cert count. Native devices are `native_paired_clients`; sum both for the total.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "pin_pending": Schema.Boolean.annotate({ "description": "Pairing handshake is waiting for a PIN (`POST /api/v1/pair/pin`)." }), "session": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "capture": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "backend_opened": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "The backend the driver opened: `nvenc` / `amf` / `qsv` / `pyrowave`. Absent as above." })), "class": Schema.String.annotate({ "description": "`healthy` / `idle` / `suspect` / `stalled` / `recovering` / `rebuilding` / `secure_desktop`." }), "cooldown_remaining_ms": Schema.optionalKey(Schema.Never), "current_stage": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "The recovery stage running now, while an episode is open." })), "detached": Schema.Number.annotate({ "description": "Encode threads the driver abandoned after a wedge; two opens the driver cycle.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "dropped_total": Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "encoder_state": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "The driver encoder's own state word: `closed` / `open` / `encoding` / `wedged`.\nAbsent until the session's first `SET_ENCODE`." })), "episodes_suppressed": Schema.Number.annotate({ "description": "Stalled verdicts refused for budget or cooldown since the last episode.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "evidence": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Activity evidence behind the verdict: `input` / `canary`." })), "last_episode": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "consecutive_failures": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "cooldown_ms": Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "recovered": Schema.Boolean, "stages": Schema.Array(CaptureStage).annotate({ "description": "The rungs run, in ladder order." }), "stall_class": Schema.String, "took_ms": Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "The last closed recovery episode." })], { mode: "oneOf" })), "late_frames": Schema.Boolean.annotate({ "description": "`present_to_arrival_ms` is past the classifier's bound: frames come late rather than not\nat all. Reported only — no recovery rung fires on it." }), "present_to_arrival_ms": Schema.optionalKey(Schema.Never), "published_total": Schema.Number.annotate({ "description": "Access units the driver published, and frames it dropped at its encode pool.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "source_gap_ms": Schema.Number.annotate({ "description": "Time since the last real source frame.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "stall_class": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "When `class` is `stalled`: `worker` / `encoder` / `presentation` / `driver`." })) }).annotate({ "description": "Live capture health (Windows IDD-push, native plane). Absent on GameStream, on Linux,\nand until the video loop's first publish." })], { mode: "oneOf" })), "fps": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "height": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "width": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "GameStream launch if present, else the first live native session. `null` when idle.\n`session_id` says which row of `sessions` this is." })], { mode: "oneOf" })), "session_id": Schema.optionalKey(Schema.Number.annotate({ "description": "Which `sessions` row `session`/`stream` describe. `null` when idle, or when the\nrepresentative is the GameStream stream (the compat plane has no id).", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))), "sessions": Schema.Array(SessionRow).annotate({ "description": "Every live session, one row each — what the per-session routes take an id from." }), "stream": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "bitrate_kbps": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "codec": ApiCodec, "fps": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "height": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "last_resize_ms": Schema.optionalKey(Schema.Never), "min_fec": Schema.Number.annotate({ "description": "Client's parity floor per FEC block (`minRequiredFecPackets`).", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "packet_size": Schema.Number.annotate({ "description": "Video payload size per packet (bytes).", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "time_to_first_frame_ms": Schema.optionalKey(Schema.Never), "width": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "Active stream parameters of that same session. `null` when idle." })], { mode: "oneOf" })), "video_streaming": Schema.Boolean }).annotate({ "description": "Live status; changes as sessions start and end." })
export type HostEvent = { readonly "client": ClientRef, readonly "kind": "client.connected", readonly "schema": number, readonly "seq": number, readonly "ts_ms": number } | { readonly "client": ClientRef, readonly "kind": "client.disconnected", readonly "reason": DisconnectReason, readonly "schema": number, readonly "seq": number, readonly "ts_ms": number } | { readonly "kind": "session.started", readonly "session": SessionRef, readonly "schema": number, readonly "seq": number, readonly "ts_ms": number } | { readonly "kind": "session.ended", readonly "session": SessionRef, readonly "summary": { readonly "audio"?: null | { readonly "infilled": number, readonly "late": number, readonly "max_late_ms": number, readonly "reanchors": number, readonly "sent": number }, readonly "bit_depth": number, readonly "bitrate"?: null | { readonly "adaptive_steps": number, readonly "avg_kbps": number, readonly "max_kbps": number, readonly "min_kbps": number }, readonly "bitrate_kbps": number, readonly "bringup_ms": number, readonly "chroma": string, readonly "client": string, readonly "client_name"?: string | null, readonly "codec": string, readonly "duration_s": number, readonly "ended": SessionEndReason, readonly "frames_dropped"?: number, readonly "frames_sent"?: number, readonly "gyro"?: null | { readonly "samples": number, readonly "stalls": number }, readonly "hdr": boolean, readonly "id": number, readonly "input": InputCounts, readonly "join": boolean, readonly "mode": string, readonly "path_mtu"?: number, readonly "started_unix": number }, readonly "schema": number, readonly "seq": number, readonly "ts_ms": number } | { readonly "kind": "stream.started", readonly "stream": StreamRef, readonly "schema": number, readonly "seq": number, readonly "ts_ms": number } | { readonly "kind": "stream.stopped", readonly "stream": StreamRef, readonly "schema": number, readonly "seq": number, readonly "ts_ms": number } | { readonly "game": GameRefPayload, readonly "kind": "game.launching", readonly "schema": number, readonly "seq": number, readonly "ts_ms": number } | { readonly "game": GameRefPayload, readonly "kind": "game.running", readonly "schema": number, readonly "seq": number, readonly "ts_ms": number } | { readonly "app_id": string, readonly "game": GameRefPayload, readonly "kind": "game.window", readonly "title": string, readonly "schema": number, readonly "seq": number, readonly "ts_ms": number } | { readonly "game": GameRefPayload, readonly "kind": "game.exited", readonly "reason": GameEndReason, readonly "schema": number, readonly "seq": number, readonly "ts_ms": number } | { readonly "device": DeviceRef, readonly "kind": "pairing.pending", readonly "schema": number, readonly "seq": number, readonly "ts_ms": number } | { readonly "device": DeviceRef, readonly "kind": "pairing.completed", readonly "schema": number, readonly "seq": number, readonly "ts_ms": number } | { readonly "device": DeviceRef, readonly "kind": "pairing.denied", readonly "schema": number, readonly "seq": number, readonly "ts_ms": number } | { readonly "device": DeviceRef, readonly "expires_unix"?: never, readonly "grants": number, readonly "kind": "access.granted", readonly "schema": number, readonly "seq": number, readonly "ts_ms": number } | { readonly "device": DeviceRef, readonly "expires_unix"?: never, readonly "grants": number, readonly "kind": "access.changed", readonly "schema": number, readonly "seq": number, readonly "ts_ms": number } | { readonly "device": DeviceRef, readonly "kind": "access.expired", readonly "schema": number, readonly "seq": number, readonly "ts_ms": number } | { readonly "backend": string, readonly "kind": "display.created", readonly "mode": string, readonly "schema": number, readonly "seq": number, readonly "ts_ms": number } | { readonly "count": number, readonly "kind": "display.released", readonly "schema": number, readonly "seq": number, readonly "ts_ms": number } | { readonly "kind": "library.changed", readonly "source": string, readonly "schema": number, readonly "seq": number, readonly "ts_ms": number } | { readonly "channel": string, readonly "install_kind": string, readonly "kind": "update.available", readonly "version": string, readonly "schema": number, readonly "seq": number, readonly "ts_ms": number } | { readonly "from": string, readonly "kind": "update.applied", readonly "to": string, readonly "schema": number, readonly "seq": number, readonly "ts_ms": number } | { readonly "id": string, readonly "kind": "plugins.changed", readonly "schema": number, readonly "seq": number, readonly "ts_ms": number } | { readonly "kind": "store.changed", readonly "schema": number, readonly "seq": number, readonly "ts_ms": number } | { readonly "ids": ReadonlyArray<string>, readonly "kind": "settings.changed", readonly "schema": number, readonly "seq": number, readonly "ts_ms": number } | { readonly "device"?: null | { readonly "fingerprint": string, readonly "name": string, readonly "plane": Plane }, readonly "id": string, readonly "kind": "action.invoked", readonly "outcome": string, readonly "schema": number, readonly "seq": number, readonly "ts_ms": number } | { readonly "gamestream": boolean, readonly "kind": "host.started", readonly "version": string, readonly "schema": number, readonly "seq": number, readonly "ts_ms": number } | { readonly "kind": "host.stopping", readonly "schema": number, readonly "seq": number, readonly "ts_ms": number }
export const HostEvent = Schema.Union([Schema.Struct({ "client": ClientRef, "kind": Schema.Literal("client.connected"), "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }), Schema.Struct({ "client": ClientRef, "kind": Schema.Literal("client.disconnected"), "reason": DisconnectReason, "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }), Schema.Struct({ "kind": Schema.Literal("session.started"), "session": SessionRef, "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }), Schema.Struct({ "kind": Schema.Literal("session.ended"), "session": SessionRef, "summary": Schema.Struct({ "audio": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "infilled": Schema.Number.annotate({ "description": "Frames synthesized over a capture hole. Wire continuity is not captured continuity.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "late": Schema.Number.annotate({ "description": "Departures a whole frame or more behind their slot.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "max_late_ms": Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "reanchors": Schema.Number.annotate({ "description": "Times the pacer fell far enough behind to forgive its debt and re-anchor.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "sent": Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "Absent when the session ran no audio plane (synthetic source, or the thread never started)." })], { mode: "oneOf" })), "bit_depth": Schema.Number.annotate({ "description": "8 or 10. Independent of `hdr` — 10-bit SDR is a mode.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "bitrate": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "adaptive_steps": Schema.Number.annotate({ "description": "Times the target moved after the opening rate: adaptive-bitrate decisions, plus a\nrebuild re-resolving what it actually encodes.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "avg_kbps": Schema.Number.annotate({ "description": "Mean of the targets the session ran at, NOT weighted by how long each held.\nA rate held for a second counts as much as one held for an hour.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "max_kbps": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "min_kbps": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "What the encoder's target did. Absent on a session that opened no encoder." })], { mode: "oneOf" })), "bitrate_kbps": Schema.Number.annotate({ "description": "The encoder's target when the session ended, kbps. `bitrate` has the span.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "bringup_ms": Schema.Number.annotate({ "description": "Hello → first video packet, ms. `0` if no packet ever left.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "chroma": Schema.String.annotate({ "description": "`4:2:0` or `4:4:4`." }), "client": Schema.String.annotate({ "description": "Cert-fingerprint prefix, or peer IP for an anonymous client." }), "client_name": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Display name (trust store, else the name the client sent). Absent if nameless." })), "codec": Schema.String.annotate({ "description": "`h264` | `hevc` | `av1` | `pyrowave`." }), "duration_s": Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ended": SessionEndReason, "frames_dropped": Schema.optionalKey(Schema.Number.annotate({ "description": "Frames the capturer's encode pool refused. Absent on a capturer that does not\ncount them (everything but the Windows IDD push path).", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))), "frames_sent": Schema.optionalKey(Schema.Number.annotate({ "description": "Access units the send thread put on the wire. Absent when the video loop\nnever reached its tail — the `host_error` case.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))), "gyro": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "samples": Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "stalls": Schema.Number.annotate({ "description": "Gaps of 500 ms or more — the feed stopping, not jitter.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "Absent when no pad sent motion — most sessions." })], { mode: "oneOf" })), "hdr": Schema.Boolean, "id": Schema.Number.annotate({ "description": "Same id [`SessionRef`] and the per-session routes carry.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "input": InputCounts, "join": Schema.Boolean.annotate({ "description": "Shared another session's display at ITS mode instead of owning one." }), "mode": Schema.String.annotate({ "description": "`WxH@Hz` as last delivered; a mid-session mode switch moves it." }), "path_mtu": Schema.optionalKey(Schema.Number.annotate({ "description": "Path MTU the QUIC stack settled on, bytes. Absent as `frames_sent` is.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))), "started_unix": Schema.Number.annotate({ "description": "Host wall clock at stream start, unix seconds.", "format": "int64" }).check(Schema.isInt()) }).annotate({ "description": "Every number the host has for the finished session, including why it ended." }), "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }), Schema.Struct({ "kind": Schema.Literal("stream.started"), "stream": StreamRef, "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }), Schema.Struct({ "kind": Schema.Literal("stream.stopped"), "stream": StreamRef, "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }), Schema.Struct({ "game": GameRefPayload, "kind": Schema.Literal("game.launching"), "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "Fires before the host spawns a launched game, never on adopt. Plugins and hooks that\nhold this stage run first ([`crate::holds`])." }), Schema.Struct({ "game": GameRefPayload, "kind": Schema.Literal("game.running"), "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "Fires once the host has seen the game process, not merely spawned its launcher." }), Schema.Struct({ "app_id": Schema.String.annotate({ "description": "Wayland `app_id` or X11 class; `steam_app_<id>` under gamescope; empty on Windows." }), "game": GameRefPayload, "kind": Schema.Literal("game.window"), "title": Schema.String.annotate({ "description": "Title the compositor or the desktop reports for that window." }), "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "Fires when the game's own window reaches the screen, which is often far\nlater than its process: a Proton prefix build, a splash on a black\nwindow, an emulator loading a ROM." }), Schema.Struct({ "game": GameRefPayload, "kind": Schema.Literal("game.exited"), "reason": GameEndReason, "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }), Schema.Struct({ "device": DeviceRef, "kind": Schema.Literal("pairing.pending"), "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }), Schema.Struct({ "device": DeviceRef, "kind": Schema.Literal("pairing.completed"), "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }), Schema.Struct({ "device": DeviceRef, "kind": Schema.Literal("pairing.denied"), "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }), Schema.Struct({ "device": DeviceRef, "expires_unix": Schema.optionalKey(Schema.Never), "grants": Schema.Number.annotate({ "description": "`GRANT_*` bits; reserved bits already cleared.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "kind": Schema.Literal("access.granted"), "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "Explicit operator choice (`add_with_access(Some)`). A pairing with no choice\nemits only `pairing.completed` — see `design/per-client-access.md`." }), Schema.Struct({ "device": DeviceRef, "expires_unix": Schema.optionalKey(Schema.Never), "grants": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "kind": Schema.Literal("access.changed"), "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "Post-pairing edit (console sheet / extend / expire-now), not the original grant." }), Schema.Struct({ "device": DeviceRef, "kind": Schema.Literal("access.expired"), "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "Deadline fire on a live session. A device with no session expires silently." }), Schema.Struct({ "backend": Schema.String.annotate({ "description": "`VirtualDisplay::name` of the backend that minted it." }), "kind": Schema.Literal("display.created"), "mode": Schema.String.annotate({ "description": "`WxH@Hz`." }), "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }), Schema.Struct({ "count": Schema.Number.annotate({ "description": "How many kept displays this release retired.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "kind": Schema.Literal("display.released"), "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }), Schema.Struct({ "kind": Schema.Literal("library.changed"), "source": Schema.String.annotate({ "description": "`\"manual\"`, or a provider id." }), "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }), Schema.Struct({ "channel": Schema.String.annotate({ "description": "`stable` or `canary`." }), "install_kind": Schema.String.annotate({ "description": "`apt`, `windows-installer`, … so a hook can hint how to update without a second call." }), "kind": Schema.Literal("update.available"), "version": Schema.String, "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "Once per discovered version; a steady \"newer exists\" does not re-fire on every refresh." }), Schema.Struct({ "from": Schema.String, "kind": Schema.Literal("update.applied"), "to": Schema.String, "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "Boot-time reconciliation by the NEW binary after a successful apply." }), Schema.Struct({ "id": Schema.String.annotate({ "description": "Plugin that registered, restarted, deregistered, or lease-expired. Re-read `GET /api/v1/plugins`." }), "kind": Schema.Literal("plugins.changed"), "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }), Schema.Struct({ "kind": Schema.Literal("store.changed"), "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "Payload-free: the store is a join. Re-read `GET /api/v1/store/catalog` / `…/installed`." }), Schema.Struct({ "ids": Schema.Array(Schema.String).annotate({ "description": "Setting ids the write named." }), "kind": Schema.Literal("settings.changed"), "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "The operator changed host settings. Re-read `GET /api/v1/host/settings`." }), Schema.Struct({ "device": Schema.optionalKey(Schema.Union([Schema.Null, Schema.Struct({ "fingerprint": Schema.String, "name": Schema.String.annotate({ "description": "Pairing-store copy, already sanitized." }), "plane": Plane }).annotate({ "description": "Cert-lane invoker; absent for the operator console (admin lane)." })], { mode: "oneOf" })), "id": Schema.String.annotate({ "description": "`power.sleep`, `power.reboot`, `power.shutdown`, `host.restart`, `display.next`." }), "kind": Schema.Literal("action.invoked"), "outcome": Schema.String.annotate({ "description": "`accepted`, or `failed: <executor error>`." }), "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "Emitted on ACCEPT, and again if the executor later fails. A succeeded power\naction ends this process, so \"accepted with no later failure\" is success." }), Schema.Struct({ "gamestream": Schema.Boolean, "kind": Schema.Literal("host.started"), "version": Schema.String, "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }), Schema.Struct({ "kind": Schema.Literal("host.stopping"), "schema": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "seq": Schema.Number.annotate({ "description": "1-based; a consumer resumes with `since = last seen`.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "ts_ms": Schema.Number.annotate({ "description": "Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) })], { mode: "oneOf" }).annotate({ "description": "Flattened as `\"kind\": \"stream.started\"` plus payload fields." })
export type EffectivePolicy = { readonly "identity": Identity, readonly "keep_alive": KeepAlive, readonly "layout": Layout, readonly "max_displays": number, readonly "mode_conflict": ModeConflict, readonly "topology": Topology }
export const EffectivePolicy = Schema.Struct({ "identity": Identity, "keep_alive": KeepAlive, "layout": Layout, "max_displays": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "mode_conflict": ModeConflict, "topology": Topology }).annotate({ "description": "The six axes after preset expansion. What lifecycle/registry read, and\nwhat mgmt echoes as in-force.\n\nEvery field is required on the wire. This type is also\n[`CustomPresetInput::fields`] (`POST/PUT /display/presets`) and a\nresponse member three times. `#[serde(default)]` would turn\n`{\"name\":\"Kiosk\",\"fields\":{}}` into a 201 storing six unchosen axes and\nmake all six optional in OpenAPI. Catalog tolerance for older entries\nlives on the read path only: [`StoredEffectivePolicy`]." })
export type PresetInfo = { readonly "fields": { readonly "identity": Identity, readonly "keep_alive": KeepAlive, readonly "layout": Layout, readonly "max_displays": number, readonly "mode_conflict": ModeConflict, readonly "topology": Topology }, readonly "id": string, readonly "summary": string }
export const PresetInfo = Schema.Struct({ "fields": Schema.Struct({ "identity": Identity, "keep_alive": KeepAlive, "layout": Layout, "max_displays": Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)), "mode_conflict": ModeConflict, "topology": Topology }).annotate({ "description": "Same fields a `Custom` policy carries." }), "id": Schema.String.annotate({ "description": "`default` | `gaming-rig` | `shared-desktop` | `hotdesk` | `workstation`." }), "summary": Schema.String }).annotate({ "description": "Picker row. `fields` is the expansion so the console does not hardcode it." })
export type DiagnosticsReport = { readonly "checks": ReadonlyArray<HostCheck>, readonly "ran_at_unix": number }
export const DiagnosticsReport = Schema.Struct({ "checks": Schema.Array(HostCheck).annotate({ "description": "Every registered check, worst-first, including `ok` and `inapplicable`. The console decides\nwhat to hide." }), "ran_at_unix": Schema.Number.annotate({ "description": "Last probe run, unix seconds.", "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0)) }).annotate({ "description": "The `GET /diagnostics` body." })
export type RecentSessions = { readonly "sessions": ReadonlyArray<SessionSummary> }
export const RecentSessions = Schema.Struct({ "sessions": Schema.Array(SessionSummary).annotate({ "description": "Newest first. Bounded in memory and lost on a host restart — this is the last\nfew sessions, not a history." }) })
export type HostSettingsState = { readonly "env_file": string, readonly "restart_pending": ReadonlyArray<string>, readonly "settings": ReadonlyArray<SettingState> }
export const HostSettingsState = Schema.Struct({ "env_file": Schema.String.annotate({ "description": "The file an operator edits to pin a setting." }), "restart_pending": Schema.Array(Schema.String).annotate({ "description": "Setting ids waiting for a restart." }), "settings": Schema.Array(SettingState).annotate({ "description": "Rows this host acts on, in page order." }) })
export type Capture = { readonly "link"?: ReadonlyArray<LinkMinute>, readonly "meta": CaptureMeta, readonly "samples": ReadonlyArray<StatsSample> }
export const Capture = Schema.Struct({ "link": Schema.optionalKey(Schema.Array(LinkMinute).annotate({ "description": "One row per minute of link health per native session, so a freeze report carries the\nloss, recovery and ABR history instead of a log ring that holds 45 minutes. Empty on a\nGameStream capture and on recordings older than this field." })), "meta": CaptureMeta, "samples": Schema.Array(StatsSample) }).annotate({ "description": "Wire and on-disk shape: summary plus sample time-series." })
export type DisplayPolicy = { readonly "capture_monitor"?: string | null, readonly "clients"?: { readonly [x: string]: ClientOverlay }, readonly "ddc_power_off"?: boolean, readonly "edid_lock"?: boolean, readonly "game_session"?: "auto" | "dedicated", readonly "identity"?: Identity, readonly "keep_alive"?: KeepAlive, readonly "keep_monitors"?: ReadonlyArray<string>, readonly "launch_workspace"?: "own" | "current", readonly "layout"?: Layout, readonly "max_displays"?: number, readonly "mode_conflict"?: ModeConflict, readonly "pnp_disable_monitors"?: boolean, readonly "preset"?: Preset, readonly "topology"?: Topology, readonly "version"?: number }
export const DisplayPolicy = Schema.Struct({ "capture_monitor": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Stream this physical connector (`DP-1`, `HDMI-A-2`) instead of a\nvirtual display; `None` is the virtual path. Host-wide, orthogonal\nto `preset`. `PUNKTFUNK_CAPTURE_MONITOR` overrides it so `host.env`\ncan pin without a console write undoing it." })), "clients": Schema.optionalKey(Schema.Record(Schema.String, ClientOverlay).annotate({ "description": "Per-device deviations, keyed by pairing fingerprint — what identity\nslots, admission and the device list already key on (never an address:\na dual-boot box keeps its fingerprint and changes its IP).\n\nWritten only through `/display/clients/{fp}`; the host-wide PUT refuses\na `clients` key outright, so a stale console cannot round-trip the\nwhole map back over a change it never saw." }).check(Schema.isPropertyNames(Schema.String))), "ddc_power_off": Schema.optionalKey(Schema.Boolean.annotate({ "description": "Windows: DDC/CI panel off (VCP 0xD6) before Exclusive isolate, on at\nrestore. Cuts standby auto-input-scan / DP link churn on a dark\nphysical. Best-effort; no DDC/CI → skip. Orthogonal to `preset`; default off." })), "edid_lock": Schema.optionalKey(Schema.Boolean.annotate({ "description": "Windows/AMD: pin connector EDID emulation while streaming\n(`pf_win_display::adl_emul`). Locked at first Exclusive isolate\nbefore physicals deactivate (awake sink answers the live-EDID read),\nunlocked at last-member teardown, crash-journaled. Inert without\n`atiadlxx.dll`. Orthogonal to `preset`; default off." })), "game_session": Schema.optionalKey(Schema.Literals(["auto", "dedicated"]).annotate({ "description": "Game-launch routing. Orthogonal to `preset`; `#[serde(default)]` is\n`Auto` so older `display-settings.json` files stay untouched." })), "identity": Schema.optionalKey(Identity), "keep_alive": Schema.optionalKey(KeepAlive), "keep_monitors": Schema.optionalKey(Schema.Array(Schema.String).annotate({ "description": "Connectors that stay lit while streaming, even under `exclusive`\n(`design/web-console-overhaul.md` §5.5).\n\nThe shared-desktop case the all-or-nothing topology axis cannot express: the\ncouch TV streams on the sole screen while the desk monitor stays usable. Only\nmeaningful under `exclusive` — nothing else turns a monitor off." })), "launch_workspace": Schema.optionalKey(Schema.Literals(["own", "current"]).annotate({ "description": "Default for a launch whose library entry names no `on_window.workspace`.\nOrthogonal to `preset`; `#[serde(default)]` is `own`, so an older\n`display-settings.json` opts in with the rest." })), "layout": Schema.optionalKey(Layout), "max_displays": Schema.optionalKey(Schema.Number.annotate({ "description": "Simultaneous live virtual displays. Clamped to `1..=16` (connector ceiling).", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))), "mode_conflict": Schema.optionalKey(ModeConflict), "pnp_disable_monitors": Schema.optionalKey(Schema.Boolean.annotate({ "description": "Windows: disable the PnP nodes of the physical monitors the isolate\nswitched off, for the stream; re-enabled at teardown. Persistent so a\nre-HPD stays off. Inactive externals are [`standby_sink_neutralise`]'s.\nA crash journal re-enables leftovers. Orthogonal to `preset`; default\non, and a v1 file migrates to on." })), "preset": Schema.optionalKey(Preset), "topology": Schema.optionalKey(Topology), "version": Schema.optionalKey(Schema.Number.annotate({ "description": "Schema version. Unknown versions load best-effort\n([`DisplayPolicyStore::load_from`] warns) and write pins current.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))) }).annotate({ "description": "File + mgmt GET/PUT shape. When [`preset`](Self::preset) is not\n[`Preset::Custom`], explicit fields are ignored; [`effective`](Self::effective)\nresolves both to [`EffectivePolicy`]." })
export type CustomPreset = { readonly "fields": EffectivePolicy, readonly "game_session"?: "auto" | "dedicated", readonly "id": string, readonly "name": string }
export const CustomPreset = Schema.Struct({ "fields": EffectivePolicy, "game_session": Schema.optionalKey(Schema.Literals(["auto", "dedicated"]).annotate({ "description": "Unlike a built-in preset, applying a custom preset sets this axis." })), "id": Schema.String.annotate({ "description": "Host-assigned, stable for the life of the entry." }), "name": Schema.String }).annotate({ "description": "Operator-named bundle of the six axes plus game-session, stored in\n`<config>/display-presets.json`. Applying one writes a `Custom`\n[`DisplayPolicy`] via `PUT /display/settings`. Editing the catalog never\nmutates the running policy; re-apply to adopt." })
export type CustomPresetInput = { readonly "fields": EffectivePolicy, readonly "game_session"?: GameSession, readonly "name": string }
export const CustomPresetInput = Schema.Struct({ "fields": EffectivePolicy, "game_session": Schema.optionalKey(GameSession), "name": Schema.String }).annotate({ "description": "Create/replace body. No `id` — the host owns it." })
export type DisplaySettingsState = { readonly "client_enforced": ReadonlyArray<string>, readonly "clients": { readonly [x: string]: ClientOverlay }, readonly "configured": boolean, readonly "custom_presets": ReadonlyArray<CustomPreset>, readonly "effective": EffectivePolicy, readonly "enforced": ReadonlyArray<string>, readonly "presets": ReadonlyArray<PresetInfo>, readonly "settings": { readonly "capture_monitor"?: string | null, readonly "clients"?: { readonly [x: string]: ClientOverlay }, readonly "ddc_power_off"?: boolean, readonly "edid_lock"?: boolean, readonly "game_session"?: "auto" | "dedicated", readonly "identity"?: Identity, readonly "keep_alive"?: KeepAlive, readonly "keep_monitors"?: ReadonlyArray<string>, readonly "launch_workspace"?: "own" | "current", readonly "layout"?: Layout, readonly "max_displays"?: number, readonly "mode_conflict"?: ModeConflict, readonly "pnp_disable_monitors"?: boolean, readonly "preset"?: Preset, readonly "topology"?: Topology, readonly "version"?: number } }
export const DisplaySettingsState = Schema.Struct({ "client_enforced": Schema.Array(Schema.String).annotate({ "description": "Overlay fields this build acts on PER DEVICE. A strict subset of\n`enforced`: an axis can be host-wide and not yet per-device, and the\nconsole must not offer a device control the host would store and ignore\n(`design/web-console-overhaul.md` D1)." }), "clients": Schema.Record(Schema.String, ClientOverlay).annotate({ "description": "Stored per-device overlays, so one fetch paints the device rows.\nREAD-ONLY here — writes go to `/display/clients/{fingerprint}`, and the\nPUT below refuses a body carrying this key." }).check(Schema.isPropertyNames(Schema.String)), "configured": Schema.Boolean.annotate({ "description": "True once `display-settings.json` exists." }), "custom_presets": Schema.Array(CustomPreset).annotate({ "description": "Saved custom presets (`display-presets.json`). Apply via a `Custom` policy of their fields." }), "effective": EffectivePolicy, "enforced": Schema.Array(Schema.String).annotate({ "description": "Names this build acts on (live vs coming-soon). Per-backend nuance is on `/display/state`." }), "presets": Schema.Array(PresetInfo), "settings": Schema.Struct({ "capture_monitor": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null]).annotate({ "description": "Stream this physical connector (`DP-1`, `HDMI-A-2`) instead of a\nvirtual display; `None` is the virtual path. Host-wide, orthogonal\nto `preset`. `PUNKTFUNK_CAPTURE_MONITOR` overrides it so `host.env`\ncan pin without a console write undoing it." })), "clients": Schema.optionalKey(Schema.Record(Schema.String, ClientOverlay).annotate({ "description": "Per-device deviations, keyed by pairing fingerprint — what identity\nslots, admission and the device list already key on (never an address:\na dual-boot box keeps its fingerprint and changes its IP).\n\nWritten only through `/display/clients/{fp}`; the host-wide PUT refuses\na `clients` key outright, so a stale console cannot round-trip the\nwhole map back over a change it never saw." }).check(Schema.isPropertyNames(Schema.String))), "ddc_power_off": Schema.optionalKey(Schema.Boolean.annotate({ "description": "Windows: DDC/CI panel off (VCP 0xD6) before Exclusive isolate, on at\nrestore. Cuts standby auto-input-scan / DP link churn on a dark\nphysical. Best-effort; no DDC/CI → skip. Orthogonal to `preset`; default off." })), "edid_lock": Schema.optionalKey(Schema.Boolean.annotate({ "description": "Windows/AMD: pin connector EDID emulation while streaming\n(`pf_win_display::adl_emul`). Locked at first Exclusive isolate\nbefore physicals deactivate (awake sink answers the live-EDID read),\nunlocked at last-member teardown, crash-journaled. Inert without\n`atiadlxx.dll`. Orthogonal to `preset`; default off." })), "game_session": Schema.optionalKey(Schema.Literals(["auto", "dedicated"]).annotate({ "description": "Game-launch routing. Orthogonal to `preset`; `#[serde(default)]` is\n`Auto` so older `display-settings.json` files stay untouched." })), "identity": Schema.optionalKey(Identity), "keep_alive": Schema.optionalKey(KeepAlive), "keep_monitors": Schema.optionalKey(Schema.Array(Schema.String).annotate({ "description": "Connectors that stay lit while streaming, even under `exclusive`\n(`design/web-console-overhaul.md` §5.5).\n\nThe shared-desktop case the all-or-nothing topology axis cannot express: the\ncouch TV streams on the sole screen while the desk monitor stays usable. Only\nmeaningful under `exclusive` — nothing else turns a monitor off." })), "launch_workspace": Schema.optionalKey(Schema.Literals(["own", "current"]).annotate({ "description": "Default for a launch whose library entry names no `on_window.workspace`.\nOrthogonal to `preset`; `#[serde(default)]` is `own`, so an older\n`display-settings.json` opts in with the rest." })), "layout": Schema.optionalKey(Layout), "max_displays": Schema.optionalKey(Schema.Number.annotate({ "description": "Simultaneous live virtual displays. Clamped to `1..=16` (connector ceiling).", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))), "mode_conflict": Schema.optionalKey(ModeConflict), "pnp_disable_monitors": Schema.optionalKey(Schema.Boolean.annotate({ "description": "Windows: disable the PnP nodes of the physical monitors the isolate\nswitched off, for the stream; re-enabled at teardown. Persistent so a\nre-HPD stays off. Inactive externals are [`standby_sink_neutralise`]'s.\nA crash journal re-enables leftovers. Orthogonal to `preset`; default\non, and a v1 file migrates to on." })), "preset": Schema.optionalKey(Preset), "topology": Schema.optionalKey(Topology), "version": Schema.optionalKey(Schema.Number.annotate({ "description": "Schema version. Unknown versions load best-effort\n([`DisplayPolicyStore::load_from`] warns) and write pins current.", "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))) }).annotate({ "description": "Stored policy, or the built-in default when unconfigured." }) }).annotate({ "description": "Stored policy, preset expansions, effective policy, and which options this build enforces." })
// schemas
export type ListActions200 = ActionList
export const ListActions200 = ActionList
export type ListActions401 = ApiError
export const ListActions401 = ApiError
export type InvokeAction200 = ActionOutcome
export const InvokeAction200 = ActionOutcome
export type InvokeAction401 = ApiError
export const InvokeAction401 = ApiError
export type InvokeAction403 = ApiError
export const InvokeAction403 = ApiError
export type InvokeAction404 = ApiError
export const InvokeAction404 = ApiError
export type InvokeAction409 = ApiError
export const InvokeAction409 = ApiError
export type InvokeAction500 = ApiError
export const InvokeAction500 = ApiError
export type InvokeAction501 = ApiError
export const InvokeAction501 = ApiError
export type PostDeviceChallenge200 = Challenge
export const PostDeviceChallenge200 = Challenge
export type PostDeviceTokenRequestJson = TokenRequest
export const PostDeviceTokenRequestJson = TokenRequest
export type PostDeviceToken200 = TokenGrant
export const PostDeviceToken200 = TokenGrant
export type PostDeviceToken401 = ApiError
export const PostDeviceToken401 = ApiError
export type ClientLogsList200 = ReadonlyArray<ClientLogMeta>
export const ClientLogsList200 = Schema.Array(ClientLogMeta)
export type ClientLogsList401 = ApiError
export const ClientLogsList401 = ApiError
export type ClientLogsUpload201 = ClientLogUploaded
export const ClientLogsUpload201 = ClientLogUploaded
export type ClientLogsUpload400 = ApiError
export const ClientLogsUpload400 = ApiError
export type ClientLogsUpload403 = ApiError
export const ClientLogsUpload403 = ApiError
export type ClientLogsUpload413 = ApiError
export const ClientLogsUpload413 = ApiError
export type ClientLogsUpload422 = ApiError
export const ClientLogsUpload422 = ApiError
export type ClientLogsUpload500 = ApiError
export const ClientLogsUpload500 = ApiError
export type ClientLogsGet401 = ApiError
export const ClientLogsGet401 = ApiError
export type ClientLogsGet404 = ApiError
export const ClientLogsGet404 = ApiError
export type ClientLogsGet500 = ApiError
export const ClientLogsGet500 = ApiError
export type ClientLogsDelete401 = ApiError
export const ClientLogsDelete401 = ApiError
export type ClientLogsDelete404 = ApiError
export const ClientLogsDelete404 = ApiError
export type ClientLogsDelete500 = ApiError
export const ClientLogsDelete500 = ApiError
export type ListPairedClients200 = ReadonlyArray<PairedClient>
export const ListPairedClients200 = Schema.Array(PairedClient)
export type ListPairedClients401 = ApiError
export const ListPairedClients401 = ApiError
export type UnpairAllClients200 = UnpairAllResult
export const UnpairAllClients200 = UnpairAllResult
export type UnpairAllClients401 = ApiError
export const UnpairAllClients401 = ApiError
export type UnpairClient400 = ApiError
export const UnpairClient400 = ApiError
export type UnpairClient401 = ApiError
export const UnpairClient401 = ApiError
export type UnpairClient404 = ApiError
export const UnpairClient404 = ApiError
export type RenameClientRequestJson = RenameClient
export const RenameClientRequestJson = RenameClient
export type RenameClient200 = PairedClient
export const RenameClient200 = PairedClient
export type RenameClient400 = ApiError
export const RenameClient400 = ApiError
export type RenameClient401 = ApiError
export const RenameClient401 = ApiError
export type RenameClient404 = ApiError
export const RenameClient404 = ApiError
export type ListCompositors200 = ReadonlyArray<AvailableCompositor>
export const ListCompositors200 = Schema.Array(AvailableCompositor)
export type ListCompositors401 = ApiError
export const ListCompositors401 = ApiError
export type GetDiagnostics200 = DiagnosticsReport
export const GetDiagnostics200 = DiagnosticsReport
export type GetDiagnostics401 = ApiError
export const GetDiagnostics401 = ApiError
export type RefreshDiagnostics200 = DiagnosticsReport
export const RefreshDiagnostics200 = DiagnosticsReport
export type RefreshDiagnostics401 = ApiError
export const RefreshDiagnostics401 = ApiError
export type GetDisplayClient200 = ClientOverlay
export const GetDisplayClient200 = ClientOverlay
export type GetDisplayClient401 = ApiError
export const GetDisplayClient401 = ApiError
export type SetDisplayClientRequestJson = ClientOverlay
export const SetDisplayClientRequestJson = ClientOverlay
export type SetDisplayClient200 = DisplaySettingsState
export const SetDisplayClient200 = DisplaySettingsState
export type SetDisplayClient400 = ApiError
export const SetDisplayClient400 = ApiError
export type SetDisplayClient401 = ApiError
export const SetDisplayClient401 = ApiError
export type SetDisplayClient500 = ApiError
export const SetDisplayClient500 = ApiError
export type DeleteDisplayClient200 = DisplaySettingsState
export const DeleteDisplayClient200 = DisplaySettingsState
export type DeleteDisplayClient401 = ApiError
export const DeleteDisplayClient401 = ApiError
export type DeleteDisplayClient500 = ApiError
export const DeleteDisplayClient500 = ApiError
export type SetDisplayLayoutRequestJson = DisplayLayoutRequest
export const SetDisplayLayoutRequestJson = DisplayLayoutRequest
export type SetDisplayLayout200 = DisplaySettingsState
export const SetDisplayLayout200 = DisplaySettingsState
export type SetDisplayLayout401 = ApiError
export const SetDisplayLayout401 = ApiError
export type SetDisplayLayout500 = ApiError
export const SetDisplayLayout500 = ApiError
export type GetDisplayMonitors200 = MonitorsResponse
export const GetDisplayMonitors200 = MonitorsResponse
export type GetDisplayMonitors401 = ApiError
export const GetDisplayMonitors401 = ApiError
export type ListCustomPresets200 = ReadonlyArray<CustomPreset>
export const ListCustomPresets200 = Schema.Array(CustomPreset)
export type ListCustomPresets401 = ApiError
export const ListCustomPresets401 = ApiError
export type CreateCustomPresetRequestJson = CustomPresetInput
export const CreateCustomPresetRequestJson = CustomPresetInput
export type CreateCustomPreset201 = CustomPreset
export const CreateCustomPreset201 = CustomPreset
export type CreateCustomPreset400 = ApiError
export const CreateCustomPreset400 = ApiError
export type CreateCustomPreset401 = ApiError
export const CreateCustomPreset401 = ApiError
export type CreateCustomPreset500 = ApiError
export const CreateCustomPreset500 = ApiError
export type UpdateCustomPresetRequestJson = CustomPresetInput
export const UpdateCustomPresetRequestJson = CustomPresetInput
export type UpdateCustomPreset200 = CustomPreset
export const UpdateCustomPreset200 = CustomPreset
export type UpdateCustomPreset400 = ApiError
export const UpdateCustomPreset400 = ApiError
export type UpdateCustomPreset401 = ApiError
export const UpdateCustomPreset401 = ApiError
export type UpdateCustomPreset404 = ApiError
export const UpdateCustomPreset404 = ApiError
export type UpdateCustomPreset500 = ApiError
export const UpdateCustomPreset500 = ApiError
export type DeleteCustomPreset401 = ApiError
export const DeleteCustomPreset401 = ApiError
export type DeleteCustomPreset404 = ApiError
export const DeleteCustomPreset404 = ApiError
export type DeleteCustomPreset500 = ApiError
export const DeleteCustomPreset500 = ApiError
export type ReleaseDisplayRequestJson = ReleaseDisplayRequest
export const ReleaseDisplayRequestJson = ReleaseDisplayRequest
export type ReleaseDisplay200 = ReleaseDisplayResult
export const ReleaseDisplay200 = ReleaseDisplayResult
export type ReleaseDisplay401 = ApiError
export const ReleaseDisplay401 = ApiError
export type GetDisplaySettings200 = DisplaySettingsState
export const GetDisplaySettings200 = DisplaySettingsState
export type GetDisplaySettings401 = ApiError
export const GetDisplaySettings401 = ApiError
export type SetDisplaySettingsRequestJson = DisplayPolicy
export const SetDisplaySettingsRequestJson = DisplayPolicy
export type SetDisplaySettings200 = DisplaySettingsState
export const SetDisplaySettings200 = DisplaySettingsState
export type SetDisplaySettings400 = ApiError
export const SetDisplaySettings400 = ApiError
export type SetDisplaySettings401 = ApiError
export const SetDisplaySettings401 = ApiError
export type SetDisplaySettings500 = ApiError
export const SetDisplaySettings500 = ApiError
export type GetDisplayState200 = DisplayStateResponse
export const GetDisplayState200 = DisplayStateResponse
export type GetDisplayState401 = ApiError
export const GetDisplayState401 = ApiError
export type StreamEventsParams = { readonly "since"?: number, readonly "kinds"?: string, readonly "Last-Event-ID"?: never }
export const StreamEventsParams = Schema.Struct({ "since": Schema.optionalKey(Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))), "kinds": Schema.optionalKey(Schema.String), "Last-Event-ID": Schema.optionalKey(Schema.Never) })
export type StreamEvents200Sse = HostEvent
export const StreamEvents200Sse = HostEvent
export type StreamEvents401 = ApiError
export const StreamEvents401 = ApiError
export type StreamEvents503 = ApiError
export const StreamEvents503 = ApiError
export type EndGameRequestJson = EndGameRequest
export const EndGameRequestJson = EndGameRequest
export type EndGame200 = EndGameResult
export const EndGame200 = EndGameResult
export type EndGame401 = ApiError
export const EndGame401 = ApiError
export type EndGame409 = ApiError
export const EndGame409 = ApiError
export type ListGpus200 = GpuState
export const ListGpus200 = GpuState
export type ListGpus401 = ApiError
export const ListGpus401 = ApiError
export type SetGpuPreferenceRequestJson = SetGpuPreference
export const SetGpuPreferenceRequestJson = SetGpuPreference
export type SetGpuPreference200 = GpuState
export const SetGpuPreference200 = GpuState
export type SetGpuPreference400 = ApiError
export const SetGpuPreference400 = ApiError
export type SetGpuPreference401 = ApiError
export const SetGpuPreference401 = ApiError
export type SetGpuPreference500 = ApiError
export const SetGpuPreference500 = ApiError
export type GetHealth200 = Health
export const GetHealth200 = Health
export type GetHooks200 = HooksConfig
export const GetHooks200 = HooksConfig
export type GetHooks401 = ApiError
export const GetHooks401 = ApiError
export type SetHooksRequestJson = HooksConfig
export const SetHooksRequestJson = HooksConfig
export type SetHooks200 = HooksConfig
export const SetHooks200 = HooksConfig
export type SetHooks400 = ApiError
export const SetHooks400 = ApiError
export type SetHooks401 = ApiError
export const SetHooks401 = ApiError
export type SetHooks500 = ApiError
export const SetHooks500 = ApiError
export type GetHostInfo200 = HostInfo
export const GetHostInfo200 = HostInfo
export type GetHostInfo401 = ApiError
export const GetHostInfo401 = ApiError
export type GetPlayingApps200 = PlayingApps
export const GetPlayingApps200 = PlayingApps
export type GetPlayingApps401 = ApiError
export const GetPlayingApps401 = ApiError
export type GetHostSettings200 = HostSettingsState
export const GetHostSettings200 = HostSettingsState
export type GetHostSettings401 = ApiError
export const GetHostSettings401 = ApiError
export type PatchHostSettingsRequestJson = HostSettingsPatch
export const PatchHostSettingsRequestJson = HostSettingsPatch
export type PatchHostSettings200 = HostSettingsState
export const PatchHostSettings200 = HostSettingsState
export type PatchHostSettings400 = ApiError
export const PatchHostSettings400 = ApiError
export type PatchHostSettings401 = ApiError
export const PatchHostSettings401 = ApiError
export type PatchHostSettings500 = ApiError
export const PatchHostSettings500 = ApiError
export type GetHostTheme200 = HostTheme
export const GetHostTheme200 = HostTheme
export type GetHostTheme401 = ApiError
export const GetHostTheme401 = ApiError
export type GetLibraryParams = { readonly "provider"?: string, readonly "platform"?: string }
export const GetLibraryParams = Schema.Struct({ "provider": Schema.optionalKey(Schema.String), "platform": Schema.optionalKey(Schema.String) })
export type GetLibrary200 = ReadonlyArray<OperatorGameEntry>
export const GetLibrary200 = Schema.Array(OperatorGameEntry)
export type GetLibrary401 = ApiError
export const GetLibrary401 = ApiError
export type GetLibraryArtParams = { readonly "If-None-Match"?: string | null }
export const GetLibraryArtParams = Schema.Struct({ "If-None-Match": Schema.optionalKey(Schema.Union([Schema.String, Schema.Null])) })
export type GetLibraryArt401 = ApiError
export const GetLibraryArt401 = ApiError
export type GetLibraryArt404 = ApiError
export const GetLibraryArt404 = ApiError
export type CreateCustomGameRequestJson = CustomInput
export const CreateCustomGameRequestJson = CustomInput
export type CreateCustomGame201 = CustomEntry
export const CreateCustomGame201 = CustomEntry
export type CreateCustomGame400 = ApiError
export const CreateCustomGame400 = ApiError
export type CreateCustomGame401 = ApiError
export const CreateCustomGame401 = ApiError
export type CreateCustomGame500 = ApiError
export const CreateCustomGame500 = ApiError
export type GetCustomGame200 = CustomEntry
export const GetCustomGame200 = CustomEntry
export type GetCustomGame401 = ApiError
export const GetCustomGame401 = ApiError
export type GetCustomGame404 = ApiError
export const GetCustomGame404 = ApiError
export type UpdateCustomGameRequestJson = CustomInput
export const UpdateCustomGameRequestJson = CustomInput
export type UpdateCustomGame200 = CustomEntry
export const UpdateCustomGame200 = CustomEntry
export type UpdateCustomGame400 = ApiError
export const UpdateCustomGame400 = ApiError
export type UpdateCustomGame401 = ApiError
export const UpdateCustomGame401 = ApiError
export type UpdateCustomGame404 = ApiError
export const UpdateCustomGame404 = ApiError
export type UpdateCustomGame500 = ApiError
export const UpdateCustomGame500 = ApiError
export type DeleteCustomGame401 = ApiError
export const DeleteCustomGame401 = ApiError
export type DeleteCustomGame404 = ApiError
export const DeleteCustomGame404 = ApiError
export type DeleteCustomGame500 = ApiError
export const DeleteCustomGame500 = ApiError
export type SetLibraryEntryHiddenRequestJson = HiddenToggle
export const SetLibraryEntryHiddenRequestJson = HiddenToggle
export type SetLibraryEntryHidden200 = HiddenState
export const SetLibraryEntryHidden200 = HiddenState
export type SetLibraryEntryHidden400 = ApiError
export const SetLibraryEntryHidden400 = ApiError
export type SetLibraryEntryHidden401 = ApiError
export const SetLibraryEntryHidden401 = ApiError
export type SetLibraryEntryHidden500 = ApiError
export const SetLibraryEntryHidden500 = ApiError
export type ListLibraryMetadata200 = ReadonlyArray<MetadataSourceInfo>
export const ListLibraryMetadata200 = Schema.Array(MetadataSourceInfo)
export type ListLibraryMetadata401 = ApiError
export const ListLibraryMetadata401 = ApiError
export type SetLibraryMetadataRequestJson = ReadonlyArray<MetadataSourceUpdate>
export const SetLibraryMetadataRequestJson = Schema.Array(MetadataSourceUpdate)
export type SetLibraryMetadata200 = ReadonlyArray<MetadataSourceInfo>
export const SetLibraryMetadata200 = Schema.Array(MetadataSourceInfo)
export type SetLibraryMetadata401 = ApiError
export const SetLibraryMetadata401 = ApiError
export type SetLibraryMetadata500 = ApiError
export const SetLibraryMetadata500 = ApiError
export type PutLibraryMetadataRequestJson = MetadataInput
export const PutLibraryMetadataRequestJson = MetadataInput
export type PutLibraryMetadata200 = MetadataAccepted
export const PutLibraryMetadata200 = MetadataAccepted
export type PutLibraryMetadata400 = ApiError
export const PutLibraryMetadata400 = ApiError
export type PutLibraryMetadata401 = ApiError
export const PutLibraryMetadata401 = ApiError
export type PutLibraryMetadata403 = ApiError
export const PutLibraryMetadata403 = ApiError
export type PutLibraryMetadata500 = ApiError
export const PutLibraryMetadata500 = ApiError
export type DeleteLibraryMetadata200 = MetadataRemoved
export const DeleteLibraryMetadata200 = MetadataRemoved
export type DeleteLibraryMetadata400 = ApiError
export const DeleteLibraryMetadata400 = ApiError
export type DeleteLibraryMetadata401 = ApiError
export const DeleteLibraryMetadata401 = ApiError
export type DeleteLibraryMetadata403 = ApiError
export const DeleteLibraryMetadata403 = ApiError
export type DeleteLibraryMetadata500 = ApiError
export const DeleteLibraryMetadata500 = ApiError
export type SetLibraryArtPickRequestJson = ArtPickInput
export const SetLibraryArtPickRequestJson = ArtPickInput
export type SetLibraryArtPick200 = Artwork
export const SetLibraryArtPick200 = Artwork
export type SetLibraryArtPick400 = ApiError
export const SetLibraryArtPick400 = ApiError
export type SetLibraryArtPick401 = ApiError
export const SetLibraryArtPick401 = ApiError
export type SetLibraryArtPick500 = ApiError
export const SetLibraryArtPick500 = ApiError
export type ReconcileProviderEntriesParams = { readonly "store"?: string }
export const ReconcileProviderEntriesParams = Schema.Struct({ "store": Schema.optionalKey(Schema.String) })
export type ReconcileProviderEntriesRequestJson = ReadonlyArray<ProviderEntryInput>
export const ReconcileProviderEntriesRequestJson = Schema.Array(ProviderEntryInput)
export type ReconcileProviderEntries200 = ReadonlyArray<CustomEntry>
export const ReconcileProviderEntries200 = Schema.Array(CustomEntry)
export type ReconcileProviderEntries400 = ApiError
export const ReconcileProviderEntries400 = ApiError
export type ReconcileProviderEntries401 = ApiError
export const ReconcileProviderEntries401 = ApiError
export type ReconcileProviderEntries409 = ApiError
export const ReconcileProviderEntries409 = ApiError
export type ReconcileProviderEntries500 = ApiError
export const ReconcileProviderEntries500 = ApiError
export type DeleteProviderEntries200 = ProviderRemoved
export const DeleteProviderEntries200 = ProviderRemoved
export type DeleteProviderEntries400 = ApiError
export const DeleteProviderEntries400 = ApiError
export type DeleteProviderEntries401 = ApiError
export const DeleteProviderEntries401 = ApiError
export type DeleteProviderEntries500 = ApiError
export const DeleteProviderEntries500 = ApiError
export type ReportProviderRunningRequestJson = ProviderRunningInput
export const ReportProviderRunningRequestJson = ProviderRunningInput
export type ReportProviderRunning200 = ProviderRunningAccepted
export const ReportProviderRunning200 = ProviderRunningAccepted
export type ReportProviderRunning400 = ApiError
export const ReportProviderRunning400 = ApiError
export type ReportProviderRunning401 = ApiError
export const ReportProviderRunning401 = ApiError
export type ReportProviderRunning403 = ApiError
export const ReportProviderRunning403 = ApiError
export type ListLibraryScanners200 = ReadonlyArray<ScannerInfo>
export const ListLibraryScanners200 = Schema.Array(ScannerInfo)
export type ListLibraryScanners401 = ApiError
export const ListLibraryScanners401 = ApiError
export type SetLibraryScannerRequestJson = ScannerToggle
export const SetLibraryScannerRequestJson = ScannerToggle
export type SetLibraryScanner200 = ReadonlyArray<ScannerInfo>
export const SetLibraryScanner200 = Schema.Array(ScannerInfo)
export type SetLibraryScanner401 = ApiError
export const SetLibraryScanner401 = ApiError
export type SetLibraryScanner403 = ApiError
export const SetLibraryScanner403 = ApiError
export type SetLibraryScanner404 = ApiError
export const SetLibraryScanner404 = ApiError
export type SetLibraryScanner500 = ApiError
export const SetLibraryScanner500 = ApiError
export type GetLocalSummary200 = LocalSummary
export const GetLocalSummary200 = LocalSummary
export type GetLocalSummary401 = ApiError
export const GetLocalSummary401 = ApiError
export type LogsGetParams = { readonly "after"?: number, readonly "limit"?: number }
export const LogsGetParams = Schema.Struct({ "after": Schema.optionalKey(Schema.Number.annotate({ "format": "int64" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))), "limit": Schema.optionalKey(Schema.Number.annotate({ "format": "int32" }).check(Schema.isInt()).check(Schema.isGreaterThanOrEqualTo(0))) })
export type LogsGet200 = LogPage
export const LogsGet200 = LogPage
export type LogsGet401 = ApiError
export const LogsGet401 = ApiError
export type ListNativeClients200 = ReadonlyArray<NativeClient>
export const ListNativeClients200 = Schema.Array(NativeClient)
export type ListNativeClients401 = ApiError
export const ListNativeClients401 = ApiError
export type UnpairAllNativeClients200 = UnpairAllResult
export const UnpairAllNativeClients200 = UnpairAllResult
export type UnpairAllNativeClients401 = ApiError
export const UnpairAllNativeClients401 = ApiError
export type UnpairAllNativeClients500 = ApiError
export const UnpairAllNativeClients500 = ApiError
export type UnpairAllNativeClients503 = ApiError
export const UnpairAllNativeClients503 = ApiError
export type UnpairNativeClient401 = ApiError
export const UnpairNativeClient401 = ApiError
export type UnpairNativeClient404 = ApiError
export const UnpairNativeClient404 = ApiError
export type UnpairNativeClient503 = ApiError
export const UnpairNativeClient503 = ApiError
export type UpdateNativeClientAccessRequestJson = UpdateNativeAccess
export const UpdateNativeClientAccessRequestJson = UpdateNativeAccess
export type UpdateNativeClientAccess200 = NativeClient
export const UpdateNativeClientAccess200 = NativeClient
export type UpdateNativeClientAccess400 = ApiError
export const UpdateNativeClientAccess400 = ApiError
export type UpdateNativeClientAccess401 = ApiError
export const UpdateNativeClientAccess401 = ApiError
export type UpdateNativeClientAccess404 = ApiError
export const UpdateNativeClientAccess404 = ApiError
export type UpdateNativeClientAccess500 = ApiError
export const UpdateNativeClientAccess500 = ApiError
export type UpdateNativeClientAccess503 = ApiError
export const UpdateNativeClientAccess503 = ApiError
export type GetNativePairing200 = NativePairStatus
export const GetNativePairing200 = NativePairStatus
export type GetNativePairing401 = ApiError
export const GetNativePairing401 = ApiError
export type DisarmNativePairing401 = ApiError
export const DisarmNativePairing401 = ApiError
export type DisarmNativePairing503 = ApiError
export const DisarmNativePairing503 = ApiError
export type ArmNativePairingRequestJson = ArmNativePairing
export const ArmNativePairingRequestJson = ArmNativePairing
export type ArmNativePairing200 = NativePairStatus
export const ArmNativePairing200 = NativePairStatus
export type ArmNativePairing400 = ApiError
export const ArmNativePairing400 = ApiError
export type ArmNativePairing401 = ApiError
export const ArmNativePairing401 = ApiError
export type ArmNativePairing503 = ApiError
export const ArmNativePairing503 = ApiError
export type ListPendingDevices200 = ReadonlyArray<PendingDevice>
export const ListPendingDevices200 = Schema.Array(PendingDevice)
export type ListPendingDevices401 = ApiError
export const ListPendingDevices401 = ApiError
export type ApprovePendingDeviceRequestJson = ApprovePending
export const ApprovePendingDeviceRequestJson = ApprovePending
export type ApprovePendingDevice200 = NativeClient
export const ApprovePendingDevice200 = NativeClient
export type ApprovePendingDevice400 = ApiError
export const ApprovePendingDevice400 = ApiError
export type ApprovePendingDevice401 = ApiError
export const ApprovePendingDevice401 = ApiError
export type ApprovePendingDevice404 = ApiError
export const ApprovePendingDevice404 = ApiError
export type ApprovePendingDevice409 = ApiError
export const ApprovePendingDevice409 = ApiError
export type ApprovePendingDevice500 = ApiError
export const ApprovePendingDevice500 = ApiError
export type ApprovePendingDevice503 = ApiError
export const ApprovePendingDevice503 = ApiError
export type DenyPendingDevice401 = ApiError
export const DenyPendingDevice401 = ApiError
export type DenyPendingDevice404 = ApiError
export const DenyPendingDevice404 = ApiError
export type DenyPendingDevice503 = ApiError
export const DenyPendingDevice503 = ApiError
export type GetPairingStatus200 = PairingStatus
export const GetPairingStatus200 = PairingStatus
export type GetPairingStatus401 = ApiError
export const GetPairingStatus401 = ApiError
export type SubmitPairingPinRequestJson = SubmitPin
export const SubmitPairingPinRequestJson = SubmitPin
export type SubmitPairingPin400 = ApiError
export const SubmitPairingPin400 = ApiError
export type SubmitPairingPin401 = ApiError
export const SubmitPairingPin401 = ApiError
export type SubmitPairingPin409 = ApiError
export const SubmitPairingPin409 = ApiError
export type SubmitPairingPin415 = ApiError
export const SubmitPairingPin415 = ApiError
export type SubmitPairingPin422 = ApiError
export const SubmitPairingPin422 = ApiError
export type GetPluginAccess200 = ReadonlyArray<PluginAccessSnapshot>
export const GetPluginAccess200 = Schema.Array(PluginAccessSnapshot)
export type GetPluginAccess401 = ApiError
export const GetPluginAccess401 = ApiError
export type GetPluginAccess403 = ApiError
export const GetPluginAccess403 = ApiError
export type GetPluginAccessRequests200 = PluginAccessSnapshot
export const GetPluginAccessRequests200 = PluginAccessSnapshot
export type GetPluginAccessRequests401 = ApiError
export const GetPluginAccessRequests401 = ApiError
export type GetPluginAccessRequests403 = ApiError
export const GetPluginAccessRequests403 = ApiError
export type RequestPluginAccessRequestJson = AccessRequest
export const RequestPluginAccessRequestJson = AccessRequest
export type RequestPluginAccess200 = ReadonlyArray<AccessPathOutcome>
export const RequestPluginAccess200 = Schema.Array(AccessPathOutcome)
export type RequestPluginAccess401 = ApiError
export const RequestPluginAccess401 = ApiError
export type RequestPluginAccess403 = ApiError
export const RequestPluginAccess403 = ApiError
export type RequestPluginAccess500 = ApiError
export const RequestPluginAccess500 = ApiError
export type DecidePluginAccessRequestJson = DecideRequest
export const DecidePluginAccessRequestJson = DecideRequest
export type DecidePluginAccess200 = PluginAccessSnapshot
export const DecidePluginAccess200 = PluginAccessSnapshot
export type DecidePluginAccess400 = ApiError
export const DecidePluginAccess400 = ApiError
export type DecidePluginAccess401 = ApiError
export const DecidePluginAccess401 = ApiError
export type DecidePluginAccess403 = ApiError
export const DecidePluginAccess403 = ApiError
export type DecidePluginAccess404 = ApiError
export const DecidePluginAccess404 = ApiError
export type DecidePluginAccess500 = ApiError
export const DecidePluginAccess500 = ApiError
export type ListPlugins200 = ReadonlyArray<PluginSummary>
export const ListPlugins200 = Schema.Array(PluginSummary)
export type ListPlugins401 = ApiError
export const ListPlugins401 = ApiError
export type IngestPluginLogsRequestJson = PluginLogBatch
export const IngestPluginLogsRequestJson = PluginLogBatch
export type IngestPluginLogs400 = ApiError
export const IngestPluginLogs400 = ApiError
export type IngestPluginLogs401 = ApiError
export const IngestPluginLogs401 = ApiError
export type RegisterPluginRequestJson = PluginRegistration
export const RegisterPluginRequestJson = PluginRegistration
export type RegisterPlugin400 = ApiError
export const RegisterPlugin400 = ApiError
export type RegisterPlugin401 = ApiError
export const RegisterPlugin401 = ApiError
export type DeregisterPlugin401 = ApiError
export const DeregisterPlugin401 = ApiError
export type GetPluginUiCredential200 = UiCredential
export const GetPluginUiCredential200 = UiCredential
export type GetPluginUiCredential401 = ApiError
export const GetPluginUiCredential401 = ApiError
export type GetPluginUiCredential404 = ApiError
export const GetPluginUiCredential404 = ApiError
export type StopSession401 = ApiError
export const StopSession401 = ApiError
export type RequestIdr401 = ApiError
export const RequestIdr401 = ApiError
export type RequestIdr409 = ApiError
export const RequestIdr409 = ApiError
export type GetRecentSessions200 = RecentSessions
export const GetRecentSessions200 = RecentSessions
export type GetRecentSessions401 = ApiError
export const GetRecentSessions401 = ApiError
export type GetSessionSettings200 = SessionSettingsState
export const GetSessionSettings200 = SessionSettingsState
export type GetSessionSettings401 = ApiError
export const GetSessionSettings401 = ApiError
export type SetSessionSettingsRequestJson = SessionSettings
export const SetSessionSettingsRequestJson = SessionSettings
export type SetSessionSettings200 = SessionSettingsState
export const SetSessionSettings200 = SessionSettingsState
export type SetSessionSettings400 = ApiError
export const SetSessionSettings400 = ApiError
export type SetSessionSettings401 = ApiError
export const SetSessionSettings401 = ApiError
export type SetSessionSettings500 = ApiError
export const SetSessionSettings500 = ApiError
export type StopOneSession401 = ApiError
export const StopOneSession401 = ApiError
export type StopOneSession404 = ApiError
export const StopOneSession404 = ApiError
export type SetSessionAccessRequestJson = SessionAccessRequest
export const SetSessionAccessRequestJson = SessionAccessRequest
export type SetSessionAccess200 = SessionAccess
export const SetSessionAccess200 = SessionAccess
export type SetSessionAccess400 = ApiError
export const SetSessionAccess400 = ApiError
export type SetSessionAccess401 = ApiError
export const SetSessionAccess401 = ApiError
export type SetSessionAccess404 = ApiError
export const SetSessionAccess404 = ApiError
export type SetSessionAudioRequestJson = SessionAudioRequest
export const SetSessionAudioRequestJson = SessionAudioRequest
export type SetSessionAudio401 = ApiError
export const SetSessionAudio401 = ApiError
export type SetSessionAudio404 = ApiError
export const SetSessionAudio404 = ApiError
export type RequestSessionIdr401 = ApiError
export const RequestSessionIdr401 = ApiError
export type RequestSessionIdr404 = ApiError
export const RequestSessionIdr404 = ApiError
export type StreamSessionPads200Sse = PadFrame
export const StreamSessionPads200Sse = PadFrame
export type StreamSessionPads401 = ApiError
export const StreamSessionPads401 = ApiError
export type StreamSessionPads404 = ApiError
export const StreamSessionPads404 = ApiError
export type StreamSessionPads503 = ApiError
export const StreamSessionPads503 = ApiError
export type SetSessionPlayerRequestJson = SessionPlayerRequest
export const SetSessionPlayerRequestJson = SessionPlayerRequest
export type SetSessionPlayer200 = SessionPlayer
export const SetSessionPlayer200 = SessionPlayer
export type SetSessionPlayer400 = ApiError
export const SetSessionPlayer400 = ApiError
export type SetSessionPlayer401 = ApiError
export const SetSessionPlayer401 = ApiError
export type SetSessionPlayer404 = ApiError
export const SetSessionPlayer404 = ApiError
export type StatsCaptureLive200 = Capture
export const StatsCaptureLive200 = Capture
export type StatsCaptureLive401 = ApiError
export const StatsCaptureLive401 = ApiError
export type StatsCaptureLive404 = ApiError
export const StatsCaptureLive404 = ApiError
export type StatsCaptureStart200 = StatsStatus
export const StatsCaptureStart200 = StatsStatus
export type StatsCaptureStart401 = ApiError
export const StatsCaptureStart401 = ApiError
export type StatsCaptureStatus200 = StatsStatus
export const StatsCaptureStatus200 = StatsStatus
export type StatsCaptureStatus401 = ApiError
export const StatsCaptureStatus401 = ApiError
export type StatsCaptureStop200 = CaptureMeta
export const StatsCaptureStop200 = CaptureMeta
export type StatsCaptureStop401 = ApiError
export const StatsCaptureStop401 = ApiError
export type StatsCaptureStop500 = ApiError
export const StatsCaptureStop500 = ApiError
export type StatsRecordingsList200 = ReadonlyArray<CaptureMeta>
export const StatsRecordingsList200 = Schema.Array(CaptureMeta)
export type StatsRecordingsList401 = ApiError
export const StatsRecordingsList401 = ApiError
export type StatsRecordingGet200 = Capture
export const StatsRecordingGet200 = Capture
export type StatsRecordingGet401 = ApiError
export const StatsRecordingGet401 = ApiError
export type StatsRecordingGet404 = ApiError
export const StatsRecordingGet404 = ApiError
export type StatsRecordingGet500 = ApiError
export const StatsRecordingGet500 = ApiError
export type StatsRecordingDelete401 = ApiError
export const StatsRecordingDelete401 = ApiError
export type StatsRecordingDelete404 = ApiError
export const StatsRecordingDelete404 = ApiError
export type StatsRecordingDelete500 = ApiError
export const StatsRecordingDelete500 = ApiError
export type GetStatus200 = RuntimeStatus
export const GetStatus200 = RuntimeStatus
export type GetStatus401 = ApiError
export const GetStatus401 = ApiError
export type GetPluginCatalog200 = CatalogResponse
export const GetPluginCatalog200 = CatalogResponse
export type GetPluginCatalog401 = ApiError
export const GetPluginCatalog401 = ApiError
export type GetPluginCatalog403 = ApiError
export const GetPluginCatalog403 = ApiError
export type InstallPluginRequestJson = InstallRequest
export const InstallPluginRequestJson = InstallRequest
export type InstallPlugin202 = JobRef
export const InstallPlugin202 = JobRef
export type InstallPlugin400 = ApiError
export const InstallPlugin400 = ApiError
export type InstallPlugin401 = ApiError
export const InstallPlugin401 = ApiError
export type InstallPlugin403 = ApiError
export const InstallPlugin403 = ApiError
export type InstallPlugin409 = ApiError
export const InstallPlugin409 = ApiError
export type ListInstalledPlugins200 = ReadonlyArray<InstalledView>
export const ListInstalledPlugins200 = Schema.Array(InstalledView)
export type ListInstalledPlugins401 = ApiError
export const ListInstalledPlugins401 = ApiError
export type ListInstalledPlugins403 = ApiError
export const ListInstalledPlugins403 = ApiError
export type ListPluginJobs200 = ReadonlyArray<Job>
export const ListPluginJobs200 = Schema.Array(Job)
export type ListPluginJobs401 = ApiError
export const ListPluginJobs401 = ApiError
export type ListPluginJobs403 = ApiError
export const ListPluginJobs403 = ApiError
export type GetPluginJob200 = Job
export const GetPluginJob200 = Job
export type GetPluginJob401 = ApiError
export const GetPluginJob401 = ApiError
export type GetPluginJob403 = ApiError
export const GetPluginJob403 = ApiError
export type GetPluginJob404 = ApiError
export const GetPluginJob404 = ApiError
export type RefreshPluginCatalog200 = CatalogResponse
export const RefreshPluginCatalog200 = CatalogResponse
export type RefreshPluginCatalog401 = ApiError
export const RefreshPluginCatalog401 = ApiError
export type RefreshPluginCatalog403 = ApiError
export const RefreshPluginCatalog403 = ApiError
export type GetPluginRuntime200 = RuntimeView
export const GetPluginRuntime200 = RuntimeView
export type GetPluginRuntime401 = ApiError
export const GetPluginRuntime401 = ApiError
export type GetPluginRuntime403 = ApiError
export const GetPluginRuntime403 = ApiError
export type SetPluginRuntimeRequestJson = RuntimeRequest
export const SetPluginRuntimeRequestJson = RuntimeRequest
export type SetPluginRuntime200 = RuntimeView
export const SetPluginRuntime200 = RuntimeView
export type SetPluginRuntime400 = ApiError
export const SetPluginRuntime400 = ApiError
export type SetPluginRuntime401 = ApiError
export const SetPluginRuntime401 = ApiError
export type SetPluginRuntime403 = ApiError
export const SetPluginRuntime403 = ApiError
export type ListPluginSources200 = ReadonlyArray<SourceView>
export const ListPluginSources200 = Schema.Array(SourceView)
export type ListPluginSources401 = ApiError
export const ListPluginSources401 = ApiError
export type ListPluginSources403 = ApiError
export const ListPluginSources403 = ApiError
export type PutPluginSourceRequestJson = SourceInput
export const PutPluginSourceRequestJson = SourceInput
export type PutPluginSource400 = ApiError
export const PutPluginSource400 = ApiError
export type PutPluginSource401 = ApiError
export const PutPluginSource401 = ApiError
export type PutPluginSource403 = ApiError
export const PutPluginSource403 = ApiError
export type DeletePluginSource401 = ApiError
export const DeletePluginSource401 = ApiError
export type DeletePluginSource403 = ApiError
export const DeletePluginSource403 = ApiError
export type UninstallPluginRequestJson = UninstallRequest
export const UninstallPluginRequestJson = UninstallRequest
export type UninstallPlugin202 = JobRef
export const UninstallPlugin202 = JobRef
export type UninstallPlugin400 = ApiError
export const UninstallPlugin400 = ApiError
export type UninstallPlugin401 = ApiError
export const UninstallPlugin401 = ApiError
export type UninstallPlugin403 = ApiError
export const UninstallPlugin403 = ApiError
export type UninstallPlugin409 = ApiError
export const UninstallPlugin409 = ApiError
export type ApplyUpdateRequestJson = ApplyRequest
export const ApplyUpdateRequestJson = ApplyRequest
export type ApplyUpdate202 = UpdateStatus
export const ApplyUpdate202 = UpdateStatus
export type ApplyUpdate401 = ApiError
export const ApplyUpdate401 = ApiError
export type ApplyUpdate409 = ApiError
export const ApplyUpdate409 = ApiError
export type ForceUpdateCheck200 = UpdateStatus
export const ForceUpdateCheck200 = UpdateStatus
export type ForceUpdateCheck401 = ApiError
export const ForceUpdateCheck401 = ApiError
export type ForceUpdateCheck409 = ApiError
export const ForceUpdateCheck409 = ApiError
export type ForceUpdateCheck429 = ApiError
export const ForceUpdateCheck429 = ApiError
export type GetUpdateStatus200 = UpdateStatus
export const GetUpdateStatus200 = UpdateStatus
export type GetUpdateStatus401 = ApiError
export const GetUpdateStatus401 = ApiError
export type GetWebTransport200 = WebTransportInfo
export const GetWebTransport200 = WebTransportInfo
export type GetWebTransport404 = ApiError
export const GetWebTransport404 = ApiError

export interface OperationConfig {
  /**
   * Whether or not the response should be included in the value returned from
   * an operation.
   *
   * If set to `true`, a tuple of `[A, HttpClientResponse]` will be returned,
   * where `A` is the success type of the operation.
   *
   * If set to `false`, only the success type of the operation will be returned.
   */
  readonly includeResponse?: boolean | undefined
}

/**
 * A utility type which optionally includes the response in the return result
 * of an operation based upon the value of the `includeResponse` configuration
 * option.
 */
export type WithOptionalResponse<A, Config extends OperationConfig> = Config extends {
  readonly includeResponse: true
} ? [A, HttpClientResponse.HttpClientResponse] : A

export const make = (
  httpClient: HttpClient.HttpClient,
  options: {
    readonly transformClient?: ((client: HttpClient.HttpClient) => Effect.Effect<HttpClient.HttpClient>) | undefined
  } = {}
): Punktfunk => {
  const unexpectedStatus = (response: HttpClientResponse.HttpClientResponse) =>
    Effect.flatMap(
      Effect.orElseSucceed(response.json, () => "Unexpected status code"),
      (description) =>
        Effect.fail(
          new HttpClientError.HttpClientError({
            reason: new HttpClientError.StatusCodeError({
              request: response.request,
              response,
              description: typeof description === "string" ? description : JSON.stringify(description),
            }),
          }),
        ),
    )
  const withResponse = <Config extends OperationConfig>(config: Config | undefined) => (
    f: (response: HttpClientResponse.HttpClientResponse) => Effect.Effect<any, any>,
  ): (request: HttpClientRequest.HttpClientRequest) => Effect.Effect<any, any> => {
    const withOptionalResponse = (
      config?.includeResponse
        ? (response: HttpClientResponse.HttpClientResponse) => Effect.map(f(response), (a) => [a, response])
        : (response: HttpClientResponse.HttpClientResponse) => f(response)
    ) as any
    return options?.transformClient
      ? (request) =>
          Effect.flatMap(
            Effect.flatMap(options.transformClient!(httpClient), (client) => client.execute(request)),
            withOptionalResponse
          )
      : (request) => Effect.flatMap(httpClient.execute(request), withOptionalResponse)
  }
  const sseRequest = <
     Type,
     DecodingServices
    >(
      schema: Schema.ConstraintDecoder<Type, DecodingServices>
    ) =>
    (
      request: HttpClientRequest.HttpClientRequest
    ): Stream.Stream<
      { readonly event: string; readonly id: string | undefined; readonly data: Type },
      HttpClientError.HttpClientError | SchemaError | Sse.Retry,
      DecodingServices
    > =>
      HttpClient.filterStatusOk(httpClient).execute(request).pipe(
        Effect.map((response) => response.stream),
        Stream.unwrap,
        Stream.decodeText(),
        Stream.pipeThroughChannel(Sse.decodeDataSchema(schema))
      )
  const decodeSuccess =
    <Schema extends Schema.Constraint>(schema: Schema) =>
    (response: HttpClientResponse.HttpClientResponse) =>
      HttpClientResponse.schemaBodyJson(schema)(response)
  const decodeError =
    <const Tag extends string, Schema extends Schema.Constraint>(tag: Tag, schema: Schema) =>
    (response: HttpClientResponse.HttpClientResponse) =>
      Effect.flatMap(
        HttpClientResponse.schemaBodyJson(schema)(response),
        (cause) => Effect.fail(PunktfunkError(tag, cause, response)),
      )
  return {
    httpClient,
    "listActions": (options) => HttpClientRequest.get(`/api/v1/actions`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(ListActions200),
      "401": decodeError("ListActions401", ListActions401),
      orElse: unexpectedStatus
    }))
  ),
    "invokeAction": (id, options) => HttpClientRequest.post(`/api/v1/actions/${id}`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(InvokeAction200),
      "401": decodeError("InvokeAction401", InvokeAction401),
      "403": decodeError("InvokeAction403", InvokeAction403),
      "404": decodeError("InvokeAction404", InvokeAction404),
      "409": decodeError("InvokeAction409", InvokeAction409),
      "500": decodeError("InvokeAction500", InvokeAction500),
      "501": decodeError("InvokeAction501", InvokeAction501),
      "202": () => Effect.void,
      orElse: unexpectedStatus
    }))
  ),
    "postDeviceChallenge": (options) => HttpClientRequest.post(`/api/v1/auth/device/challenge`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(PostDeviceChallenge200),
      orElse: unexpectedStatus
    }))
  ),
    "postDeviceToken": (options) => HttpClientRequest.post(`/api/v1/auth/device/token`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(PostDeviceToken200),
      "401": decodeError("PostDeviceToken401", PostDeviceToken401),
      orElse: unexpectedStatus
    }))
  ),
    "clientLogsList": (options) => HttpClientRequest.get(`/api/v1/client-logs`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(ClientLogsList200),
      "401": decodeError("ClientLogsList401", ClientLogsList401),
      orElse: unexpectedStatus
    }))
  ),
    "clientLogsUpload": (options) => HttpClientRequest.post(`/api/v1/client-logs`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(ClientLogsUpload201),
      "400": decodeError("ClientLogsUpload400", ClientLogsUpload400),
      "403": decodeError("ClientLogsUpload403", ClientLogsUpload403),
      "413": decodeError("ClientLogsUpload413", ClientLogsUpload413),
      "422": decodeError("ClientLogsUpload422", ClientLogsUpload422),
      "500": decodeError("ClientLogsUpload500", ClientLogsUpload500),
      orElse: unexpectedStatus
    }))
  ),
    "clientLogsGet": (id, options) => HttpClientRequest.get(`/api/v1/client-logs/${id}`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "401": decodeError("ClientLogsGet401", ClientLogsGet401),
      "404": decodeError("ClientLogsGet404", ClientLogsGet404),
      "500": decodeError("ClientLogsGet500", ClientLogsGet500),
      orElse: unexpectedStatus
    }))
  ),
    "clientLogsDelete": (id, options) => HttpClientRequest.delete(`/api/v1/client-logs/${id}`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "401": decodeError("ClientLogsDelete401", ClientLogsDelete401),
      "404": decodeError("ClientLogsDelete404", ClientLogsDelete404),
      "500": decodeError("ClientLogsDelete500", ClientLogsDelete500),
      "204": () => Effect.void,
      orElse: unexpectedStatus
    }))
  ),
    "listPairedClients": (options) => HttpClientRequest.get(`/api/v1/clients`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(ListPairedClients200),
      "401": decodeError("ListPairedClients401", ListPairedClients401),
      orElse: unexpectedStatus
    }))
  ),
    "unpairAllClients": (options) => HttpClientRequest.delete(`/api/v1/clients`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(UnpairAllClients200),
      "401": decodeError("UnpairAllClients401", UnpairAllClients401),
      orElse: unexpectedStatus
    }))
  ),
    "unpairClient": (fingerprint, options) => HttpClientRequest.delete(`/api/v1/clients/${fingerprint}`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "400": decodeError("UnpairClient400", UnpairClient400),
      "401": decodeError("UnpairClient401", UnpairClient401),
      "404": decodeError("UnpairClient404", UnpairClient404),
      "204": () => Effect.void,
      orElse: unexpectedStatus
    }))
  ),
    "renameClient": (fingerprint, options) => HttpClientRequest.patch(`/api/v1/clients/${fingerprint}`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(RenameClient200),
      "400": decodeError("RenameClient400", RenameClient400),
      "401": decodeError("RenameClient401", RenameClient401),
      "404": decodeError("RenameClient404", RenameClient404),
      orElse: unexpectedStatus
    }))
  ),
    "listCompositors": (options) => HttpClientRequest.get(`/api/v1/compositors`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(ListCompositors200),
      "401": decodeError("ListCompositors401", ListCompositors401),
      orElse: unexpectedStatus
    }))
  ),
    "getDiagnostics": (options) => HttpClientRequest.get(`/api/v1/diagnostics`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetDiagnostics200),
      "401": decodeError("GetDiagnostics401", GetDiagnostics401),
      orElse: unexpectedStatus
    }))
  ),
    "refreshDiagnostics": (options) => HttpClientRequest.post(`/api/v1/diagnostics/refresh`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(RefreshDiagnostics200),
      "401": decodeError("RefreshDiagnostics401", RefreshDiagnostics401),
      orElse: unexpectedStatus
    }))
  ),
    "getDisplayClient": (fingerprint, options) => HttpClientRequest.get(`/api/v1/display/clients/${fingerprint}`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetDisplayClient200),
      "401": decodeError("GetDisplayClient401", GetDisplayClient401),
      orElse: unexpectedStatus
    }))
  ),
    "setDisplayClient": (fingerprint, options) => HttpClientRequest.put(`/api/v1/display/clients/${fingerprint}`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(SetDisplayClient200),
      "400": decodeError("SetDisplayClient400", SetDisplayClient400),
      "401": decodeError("SetDisplayClient401", SetDisplayClient401),
      "500": decodeError("SetDisplayClient500", SetDisplayClient500),
      orElse: unexpectedStatus
    }))
  ),
    "deleteDisplayClient": (fingerprint, options) => HttpClientRequest.delete(`/api/v1/display/clients/${fingerprint}`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(DeleteDisplayClient200),
      "401": decodeError("DeleteDisplayClient401", DeleteDisplayClient401),
      "500": decodeError("DeleteDisplayClient500", DeleteDisplayClient500),
      orElse: unexpectedStatus
    }))
  ),
    "setDisplayLayout": (options) => HttpClientRequest.put(`/api/v1/display/layout`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(SetDisplayLayout200),
      "401": decodeError("SetDisplayLayout401", SetDisplayLayout401),
      "500": decodeError("SetDisplayLayout500", SetDisplayLayout500),
      orElse: unexpectedStatus
    }))
  ),
    "getDisplayMonitors": (options) => HttpClientRequest.get(`/api/v1/display/monitors`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetDisplayMonitors200),
      "401": decodeError("GetDisplayMonitors401", GetDisplayMonitors401),
      orElse: unexpectedStatus
    }))
  ),
    "listCustomPresets": (options) => HttpClientRequest.get(`/api/v1/display/presets`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(ListCustomPresets200),
      "401": decodeError("ListCustomPresets401", ListCustomPresets401),
      orElse: unexpectedStatus
    }))
  ),
    "createCustomPreset": (options) => HttpClientRequest.post(`/api/v1/display/presets`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(CreateCustomPreset201),
      "400": decodeError("CreateCustomPreset400", CreateCustomPreset400),
      "401": decodeError("CreateCustomPreset401", CreateCustomPreset401),
      "500": decodeError("CreateCustomPreset500", CreateCustomPreset500),
      orElse: unexpectedStatus
    }))
  ),
    "updateCustomPreset": (id, options) => HttpClientRequest.put(`/api/v1/display/presets/${id}`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(UpdateCustomPreset200),
      "400": decodeError("UpdateCustomPreset400", UpdateCustomPreset400),
      "401": decodeError("UpdateCustomPreset401", UpdateCustomPreset401),
      "404": decodeError("UpdateCustomPreset404", UpdateCustomPreset404),
      "500": decodeError("UpdateCustomPreset500", UpdateCustomPreset500),
      orElse: unexpectedStatus
    }))
  ),
    "deleteCustomPreset": (id, options) => HttpClientRequest.delete(`/api/v1/display/presets/${id}`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "401": decodeError("DeleteCustomPreset401", DeleteCustomPreset401),
      "404": decodeError("DeleteCustomPreset404", DeleteCustomPreset404),
      "500": decodeError("DeleteCustomPreset500", DeleteCustomPreset500),
      "204": () => Effect.void,
      orElse: unexpectedStatus
    }))
  ),
    "releaseDisplay": (options) => HttpClientRequest.post(`/api/v1/display/release`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(ReleaseDisplay200),
      "401": decodeError("ReleaseDisplay401", ReleaseDisplay401),
      orElse: unexpectedStatus
    }))
  ),
    "getDisplaySettings": (options) => HttpClientRequest.get(`/api/v1/display/settings`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetDisplaySettings200),
      "401": decodeError("GetDisplaySettings401", GetDisplaySettings401),
      orElse: unexpectedStatus
    }))
  ),
    "setDisplaySettings": (options) => HttpClientRequest.put(`/api/v1/display/settings`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(SetDisplaySettings200),
      "400": decodeError("SetDisplaySettings400", SetDisplaySettings400),
      "401": decodeError("SetDisplaySettings401", SetDisplaySettings401),
      "500": decodeError("SetDisplaySettings500", SetDisplaySettings500),
      orElse: unexpectedStatus
    }))
  ),
    "getDisplayState": (options) => HttpClientRequest.get(`/api/v1/display/state`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetDisplayState200),
      "401": decodeError("GetDisplayState401", GetDisplayState401),
      orElse: unexpectedStatus
    }))
  ),
    "streamEvents": (options) => HttpClientRequest.get(`/api/v1/events`).pipe(
    HttpClientRequest.setUrlParams({ "since": options?.params?.["since"] as any, "kinds": options?.params?.["kinds"] as any }),
    HttpClientRequest.setHeaders({ "Last-Event-ID": options?.params?.["Last-Event-ID"] ?? undefined }),
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "401": decodeError("StreamEvents401", StreamEvents401),
      "503": decodeError("StreamEvents503", StreamEvents503),
      orElse: unexpectedStatus
    }))
  ),
    "streamEventsSse": (options) => HttpClientRequest.get(`/api/v1/events`).pipe(
      HttpClientRequest.setUrlParams({ "since": options?.params?.["since"] as any, "kinds": options?.params?.["kinds"] as any }),
      HttpClientRequest.setHeaders({ "Last-Event-ID": options?.params?.["Last-Event-ID"] ?? undefined }),
      sseRequest(StreamEvents200Sse)
    ),
    "endGame": (options) => HttpClientRequest.post(`/api/v1/game/end`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(EndGame200),
      "401": decodeError("EndGame401", EndGame401),
      "409": decodeError("EndGame409", EndGame409),
      orElse: unexpectedStatus
    }))
  ),
    "listGpus": (options) => HttpClientRequest.get(`/api/v1/gpus`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(ListGpus200),
      "401": decodeError("ListGpus401", ListGpus401),
      orElse: unexpectedStatus
    }))
  ),
    "setGpuPreference": (options) => HttpClientRequest.put(`/api/v1/gpus/preference`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(SetGpuPreference200),
      "400": decodeError("SetGpuPreference400", SetGpuPreference400),
      "401": decodeError("SetGpuPreference401", SetGpuPreference401),
      "500": decodeError("SetGpuPreference500", SetGpuPreference500),
      orElse: unexpectedStatus
    }))
  ),
    "getHealth": (options) => HttpClientRequest.get(`/api/v1/health`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetHealth200),
      orElse: unexpectedStatus
    }))
  ),
    "getHooks": (options) => HttpClientRequest.get(`/api/v1/hooks`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetHooks200),
      "401": decodeError("GetHooks401", GetHooks401),
      orElse: unexpectedStatus
    }))
  ),
    "setHooks": (options) => HttpClientRequest.put(`/api/v1/hooks`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(SetHooks200),
      "400": decodeError("SetHooks400", SetHooks400),
      "401": decodeError("SetHooks401", SetHooks401),
      "500": decodeError("SetHooks500", SetHooks500),
      orElse: unexpectedStatus
    }))
  ),
    "getHostInfo": (options) => HttpClientRequest.get(`/api/v1/host`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetHostInfo200),
      "401": decodeError("GetHostInfo401", GetHostInfo401),
      orElse: unexpectedStatus
    }))
  ),
    "getPlayingApps": (options) => HttpClientRequest.get(`/api/v1/host/audio/apps`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetPlayingApps200),
      "401": decodeError("GetPlayingApps401", GetPlayingApps401),
      orElse: unexpectedStatus
    }))
  ),
    "getHostSettings": (options) => HttpClientRequest.get(`/api/v1/host/settings`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetHostSettings200),
      "401": decodeError("GetHostSettings401", GetHostSettings401),
      orElse: unexpectedStatus
    }))
  ),
    "patchHostSettings": (options) => HttpClientRequest.patch(`/api/v1/host/settings`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(PatchHostSettings200),
      "400": decodeError("PatchHostSettings400", PatchHostSettings400),
      "401": decodeError("PatchHostSettings401", PatchHostSettings401),
      "500": decodeError("PatchHostSettings500", PatchHostSettings500),
      orElse: unexpectedStatus
    }))
  ),
    "getHostTheme": (options) => HttpClientRequest.get(`/api/v1/host/theme`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetHostTheme200),
      "401": decodeError("GetHostTheme401", GetHostTheme401),
      orElse: unexpectedStatus
    }))
  ),
    "getLibrary": (options) => HttpClientRequest.get(`/api/v1/library`).pipe(
    HttpClientRequest.setUrlParams({ "provider": options?.params?.["provider"] as any, "platform": options?.params?.["platform"] as any }),
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetLibrary200),
      "401": decodeError("GetLibrary401", GetLibrary401),
      orElse: unexpectedStatus
    }))
  ),
    "getLibraryArt": (id, kind, options) => HttpClientRequest.get(`/api/v1/library/art/${id}/${kind}`).pipe(
    HttpClientRequest.setHeaders({ "If-None-Match": options?.params?.["If-None-Match"] ?? undefined }),
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "401": decodeError("GetLibraryArt401", GetLibraryArt401),
      "404": decodeError("GetLibraryArt404", GetLibraryArt404),
      "304": () => Effect.void,
      orElse: unexpectedStatus
    }))
  ),
    "createCustomGame": (options) => HttpClientRequest.post(`/api/v1/library/custom`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(CreateCustomGame201),
      "400": decodeError("CreateCustomGame400", CreateCustomGame400),
      "401": decodeError("CreateCustomGame401", CreateCustomGame401),
      "500": decodeError("CreateCustomGame500", CreateCustomGame500),
      orElse: unexpectedStatus
    }))
  ),
    "getCustomGame": (id, options) => HttpClientRequest.get(`/api/v1/library/custom/${id}`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetCustomGame200),
      "401": decodeError("GetCustomGame401", GetCustomGame401),
      "404": decodeError("GetCustomGame404", GetCustomGame404),
      orElse: unexpectedStatus
    }))
  ),
    "updateCustomGame": (id, options) => HttpClientRequest.put(`/api/v1/library/custom/${id}`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(UpdateCustomGame200),
      "400": decodeError("UpdateCustomGame400", UpdateCustomGame400),
      "401": decodeError("UpdateCustomGame401", UpdateCustomGame401),
      "404": decodeError("UpdateCustomGame404", UpdateCustomGame404),
      "500": decodeError("UpdateCustomGame500", UpdateCustomGame500),
      orElse: unexpectedStatus
    }))
  ),
    "deleteCustomGame": (id, options) => HttpClientRequest.delete(`/api/v1/library/custom/${id}`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "401": decodeError("DeleteCustomGame401", DeleteCustomGame401),
      "404": decodeError("DeleteCustomGame404", DeleteCustomGame404),
      "500": decodeError("DeleteCustomGame500", DeleteCustomGame500),
      "204": () => Effect.void,
      orElse: unexpectedStatus
    }))
  ),
    "setLibraryEntryHidden": (id, options) => HttpClientRequest.put(`/api/v1/library/hidden/${id}`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(SetLibraryEntryHidden200),
      "400": decodeError("SetLibraryEntryHidden400", SetLibraryEntryHidden400),
      "401": decodeError("SetLibraryEntryHidden401", SetLibraryEntryHidden401),
      "500": decodeError("SetLibraryEntryHidden500", SetLibraryEntryHidden500),
      orElse: unexpectedStatus
    }))
  ),
    "listLibraryMetadata": (options) => HttpClientRequest.get(`/api/v1/library/metadata`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(ListLibraryMetadata200),
      "401": decodeError("ListLibraryMetadata401", ListLibraryMetadata401),
      orElse: unexpectedStatus
    }))
  ),
    "setLibraryMetadata": (options) => HttpClientRequest.put(`/api/v1/library/metadata`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(SetLibraryMetadata200),
      "401": decodeError("SetLibraryMetadata401", SetLibraryMetadata401),
      "500": decodeError("SetLibraryMetadata500", SetLibraryMetadata500),
      orElse: unexpectedStatus
    }))
  ),
    "putLibraryMetadata": (source, options) => HttpClientRequest.put(`/api/v1/library/metadata/${source}`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(PutLibraryMetadata200),
      "400": decodeError("PutLibraryMetadata400", PutLibraryMetadata400),
      "401": decodeError("PutLibraryMetadata401", PutLibraryMetadata401),
      "403": decodeError("PutLibraryMetadata403", PutLibraryMetadata403),
      "500": decodeError("PutLibraryMetadata500", PutLibraryMetadata500),
      orElse: unexpectedStatus
    }))
  ),
    "deleteLibraryMetadata": (source, options) => HttpClientRequest.delete(`/api/v1/library/metadata/${source}`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(DeleteLibraryMetadata200),
      "400": decodeError("DeleteLibraryMetadata400", DeleteLibraryMetadata400),
      "401": decodeError("DeleteLibraryMetadata401", DeleteLibraryMetadata401),
      "403": decodeError("DeleteLibraryMetadata403", DeleteLibraryMetadata403),
      "500": decodeError("DeleteLibraryMetadata500", DeleteLibraryMetadata500),
      orElse: unexpectedStatus
    }))
  ),
    "setLibraryArtPick": (id, options) => HttpClientRequest.put(`/api/v1/library/picks/${id}`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(SetLibraryArtPick200),
      "400": decodeError("SetLibraryArtPick400", SetLibraryArtPick400),
      "401": decodeError("SetLibraryArtPick401", SetLibraryArtPick401),
      "500": decodeError("SetLibraryArtPick500", SetLibraryArtPick500),
      orElse: unexpectedStatus
    }))
  ),
    "reconcileProviderEntries": (provider, options) => HttpClientRequest.put(`/api/v1/library/provider/${provider}`).pipe(
    HttpClientRequest.setUrlParams({ "store": options.params?.["store"] as any }),
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(ReconcileProviderEntries200),
      "400": decodeError("ReconcileProviderEntries400", ReconcileProviderEntries400),
      "401": decodeError("ReconcileProviderEntries401", ReconcileProviderEntries401),
      "409": decodeError("ReconcileProviderEntries409", ReconcileProviderEntries409),
      "500": decodeError("ReconcileProviderEntries500", ReconcileProviderEntries500),
      orElse: unexpectedStatus
    }))
  ),
    "deleteProviderEntries": (provider, options) => HttpClientRequest.delete(`/api/v1/library/provider/${provider}`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(DeleteProviderEntries200),
      "400": decodeError("DeleteProviderEntries400", DeleteProviderEntries400),
      "401": decodeError("DeleteProviderEntries401", DeleteProviderEntries401),
      "500": decodeError("DeleteProviderEntries500", DeleteProviderEntries500),
      orElse: unexpectedStatus
    }))
  ),
    "reportProviderRunning": (provider, options) => HttpClientRequest.put(`/api/v1/library/provider/${provider}/running`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(ReportProviderRunning200),
      "400": decodeError("ReportProviderRunning400", ReportProviderRunning400),
      "401": decodeError("ReportProviderRunning401", ReportProviderRunning401),
      "403": decodeError("ReportProviderRunning403", ReportProviderRunning403),
      orElse: unexpectedStatus
    }))
  ),
    "listLibraryScanners": (options) => HttpClientRequest.get(`/api/v1/library/scanners`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(ListLibraryScanners200),
      "401": decodeError("ListLibraryScanners401", ListLibraryScanners401),
      orElse: unexpectedStatus
    }))
  ),
    "setLibraryScanner": (id, options) => HttpClientRequest.put(`/api/v1/library/scanners/${id}`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(SetLibraryScanner200),
      "401": decodeError("SetLibraryScanner401", SetLibraryScanner401),
      "403": decodeError("SetLibraryScanner403", SetLibraryScanner403),
      "404": decodeError("SetLibraryScanner404", SetLibraryScanner404),
      "500": decodeError("SetLibraryScanner500", SetLibraryScanner500),
      orElse: unexpectedStatus
    }))
  ),
    "getLocalSummary": (options) => HttpClientRequest.get(`/api/v1/local/summary`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetLocalSummary200),
      "401": decodeError("GetLocalSummary401", GetLocalSummary401),
      orElse: unexpectedStatus
    }))
  ),
    "logsGet": (options) => HttpClientRequest.get(`/api/v1/logs`).pipe(
    HttpClientRequest.setUrlParams({ "after": options?.params?.["after"] as any, "limit": options?.params?.["limit"] as any }),
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(LogsGet200),
      "401": decodeError("LogsGet401", LogsGet401),
      orElse: unexpectedStatus
    }))
  ),
    "listNativeClients": (options) => HttpClientRequest.get(`/api/v1/native/clients`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(ListNativeClients200),
      "401": decodeError("ListNativeClients401", ListNativeClients401),
      orElse: unexpectedStatus
    }))
  ),
    "unpairAllNativeClients": (options) => HttpClientRequest.delete(`/api/v1/native/clients`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(UnpairAllNativeClients200),
      "401": decodeError("UnpairAllNativeClients401", UnpairAllNativeClients401),
      "500": decodeError("UnpairAllNativeClients500", UnpairAllNativeClients500),
      "503": decodeError("UnpairAllNativeClients503", UnpairAllNativeClients503),
      orElse: unexpectedStatus
    }))
  ),
    "unpairNativeClient": (fingerprint, options) => HttpClientRequest.delete(`/api/v1/native/clients/${fingerprint}`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "401": decodeError("UnpairNativeClient401", UnpairNativeClient401),
      "404": decodeError("UnpairNativeClient404", UnpairNativeClient404),
      "503": decodeError("UnpairNativeClient503", UnpairNativeClient503),
      "204": () => Effect.void,
      orElse: unexpectedStatus
    }))
  ),
    "updateNativeClientAccess": (fingerprint, options) => HttpClientRequest.patch(`/api/v1/native/clients/${fingerprint}`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(UpdateNativeClientAccess200),
      "400": decodeError("UpdateNativeClientAccess400", UpdateNativeClientAccess400),
      "401": decodeError("UpdateNativeClientAccess401", UpdateNativeClientAccess401),
      "404": decodeError("UpdateNativeClientAccess404", UpdateNativeClientAccess404),
      "500": decodeError("UpdateNativeClientAccess500", UpdateNativeClientAccess500),
      "503": decodeError("UpdateNativeClientAccess503", UpdateNativeClientAccess503),
      orElse: unexpectedStatus
    }))
  ),
    "getNativePairing": (options) => HttpClientRequest.get(`/api/v1/native/pair`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetNativePairing200),
      "401": decodeError("GetNativePairing401", GetNativePairing401),
      orElse: unexpectedStatus
    }))
  ),
    "disarmNativePairing": (options) => HttpClientRequest.delete(`/api/v1/native/pair`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "401": decodeError("DisarmNativePairing401", DisarmNativePairing401),
      "503": decodeError("DisarmNativePairing503", DisarmNativePairing503),
      "204": () => Effect.void,
      orElse: unexpectedStatus
    }))
  ),
    "armNativePairing": (options) => HttpClientRequest.post(`/api/v1/native/pair/arm`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(ArmNativePairing200),
      "400": decodeError("ArmNativePairing400", ArmNativePairing400),
      "401": decodeError("ArmNativePairing401", ArmNativePairing401),
      "503": decodeError("ArmNativePairing503", ArmNativePairing503),
      orElse: unexpectedStatus
    }))
  ),
    "listPendingDevices": (options) => HttpClientRequest.get(`/api/v1/native/pending`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(ListPendingDevices200),
      "401": decodeError("ListPendingDevices401", ListPendingDevices401),
      orElse: unexpectedStatus
    }))
  ),
    "approvePendingDevice": (id, options) => HttpClientRequest.post(`/api/v1/native/pending/${id}/approve`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(ApprovePendingDevice200),
      "400": decodeError("ApprovePendingDevice400", ApprovePendingDevice400),
      "401": decodeError("ApprovePendingDevice401", ApprovePendingDevice401),
      "404": decodeError("ApprovePendingDevice404", ApprovePendingDevice404),
      "409": decodeError("ApprovePendingDevice409", ApprovePendingDevice409),
      "500": decodeError("ApprovePendingDevice500", ApprovePendingDevice500),
      "503": decodeError("ApprovePendingDevice503", ApprovePendingDevice503),
      orElse: unexpectedStatus
    }))
  ),
    "denyPendingDevice": (id, options) => HttpClientRequest.post(`/api/v1/native/pending/${id}/deny`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "401": decodeError("DenyPendingDevice401", DenyPendingDevice401),
      "404": decodeError("DenyPendingDevice404", DenyPendingDevice404),
      "503": decodeError("DenyPendingDevice503", DenyPendingDevice503),
      "204": () => Effect.void,
      orElse: unexpectedStatus
    }))
  ),
    "getPairingStatus": (options) => HttpClientRequest.get(`/api/v1/pair`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetPairingStatus200),
      "401": decodeError("GetPairingStatus401", GetPairingStatus401),
      orElse: unexpectedStatus
    }))
  ),
    "submitPairingPin": (options) => HttpClientRequest.post(`/api/v1/pair/pin`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "400": decodeError("SubmitPairingPin400", SubmitPairingPin400),
      "401": decodeError("SubmitPairingPin401", SubmitPairingPin401),
      "409": decodeError("SubmitPairingPin409", SubmitPairingPin409),
      "415": decodeError("SubmitPairingPin415", SubmitPairingPin415),
      "422": decodeError("SubmitPairingPin422", SubmitPairingPin422),
      "204": () => Effect.void,
      orElse: unexpectedStatus
    }))
  ),
    "getPluginAccess": (options) => HttpClientRequest.get(`/api/v1/plugin-access`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetPluginAccess200),
      "401": decodeError("GetPluginAccess401", GetPluginAccess401),
      "403": decodeError("GetPluginAccess403", GetPluginAccess403),
      orElse: unexpectedStatus
    }))
  ),
    "getPluginAccessRequests": (options) => HttpClientRequest.get(`/api/v1/plugin-access/requests`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetPluginAccessRequests200),
      "401": decodeError("GetPluginAccessRequests401", GetPluginAccessRequests401),
      "403": decodeError("GetPluginAccessRequests403", GetPluginAccessRequests403),
      orElse: unexpectedStatus
    }))
  ),
    "requestPluginAccess": (options) => HttpClientRequest.post(`/api/v1/plugin-access/requests`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(RequestPluginAccess200),
      "401": decodeError("RequestPluginAccess401", RequestPluginAccess401),
      "403": decodeError("RequestPluginAccess403", RequestPluginAccess403),
      "500": decodeError("RequestPluginAccess500", RequestPluginAccess500),
      orElse: unexpectedStatus
    }))
  ),
    "decidePluginAccess": (plugin, options) => HttpClientRequest.post(`/api/v1/plugin-access/${plugin}/decide`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(DecidePluginAccess200),
      "400": decodeError("DecidePluginAccess400", DecidePluginAccess400),
      "401": decodeError("DecidePluginAccess401", DecidePluginAccess401),
      "403": decodeError("DecidePluginAccess403", DecidePluginAccess403),
      "404": decodeError("DecidePluginAccess404", DecidePluginAccess404),
      "500": decodeError("DecidePluginAccess500", DecidePluginAccess500),
      orElse: unexpectedStatus
    }))
  ),
    "listPlugins": (options) => HttpClientRequest.get(`/api/v1/plugins`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(ListPlugins200),
      "401": decodeError("ListPlugins401", ListPlugins401),
      orElse: unexpectedStatus
    }))
  ),
    "ingestPluginLogs": (options) => HttpClientRequest.post(`/api/v1/plugins/logs`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "400": decodeError("IngestPluginLogs400", IngestPluginLogs400),
      "401": decodeError("IngestPluginLogs401", IngestPluginLogs401),
      "204": () => Effect.void,
      orElse: unexpectedStatus
    }))
  ),
    "registerPlugin": (id, options) => HttpClientRequest.put(`/api/v1/plugins/${id}`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "400": decodeError("RegisterPlugin400", RegisterPlugin400),
      "401": decodeError("RegisterPlugin401", RegisterPlugin401),
      "204": () => Effect.void,
      orElse: unexpectedStatus
    }))
  ),
    "deregisterPlugin": (id, options) => HttpClientRequest.delete(`/api/v1/plugins/${id}`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "401": decodeError("DeregisterPlugin401", DeregisterPlugin401),
      "204": () => Effect.void,
      orElse: unexpectedStatus
    }))
  ),
    "getPluginUiCredential": (id, options) => HttpClientRequest.get(`/api/v1/plugins/${id}/ui-credential`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetPluginUiCredential200),
      "401": decodeError("GetPluginUiCredential401", GetPluginUiCredential401),
      "404": decodeError("GetPluginUiCredential404", GetPluginUiCredential404),
      orElse: unexpectedStatus
    }))
  ),
    "stopSession": (options) => HttpClientRequest.delete(`/api/v1/session`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "401": decodeError("StopSession401", StopSession401),
      "204": () => Effect.void,
      orElse: unexpectedStatus
    }))
  ),
    "requestIdr": (options) => HttpClientRequest.post(`/api/v1/session/idr`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "401": decodeError("RequestIdr401", RequestIdr401),
      "409": decodeError("RequestIdr409", RequestIdr409),
      "202": () => Effect.void,
      orElse: unexpectedStatus
    }))
  ),
    "getRecentSessions": (options) => HttpClientRequest.get(`/api/v1/session/last`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetRecentSessions200),
      "401": decodeError("GetRecentSessions401", GetRecentSessions401),
      orElse: unexpectedStatus
    }))
  ),
    "getSessionSettings": (options) => HttpClientRequest.get(`/api/v1/session/settings`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetSessionSettings200),
      "401": decodeError("GetSessionSettings401", GetSessionSettings401),
      orElse: unexpectedStatus
    }))
  ),
    "setSessionSettings": (options) => HttpClientRequest.put(`/api/v1/session/settings`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(SetSessionSettings200),
      "400": decodeError("SetSessionSettings400", SetSessionSettings400),
      "401": decodeError("SetSessionSettings401", SetSessionSettings401),
      "500": decodeError("SetSessionSettings500", SetSessionSettings500),
      orElse: unexpectedStatus
    }))
  ),
    "stopOneSession": (id, options) => HttpClientRequest.delete(`/api/v1/session/${id}`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "401": decodeError("StopOneSession401", StopOneSession401),
      "404": decodeError("StopOneSession404", StopOneSession404),
      "204": () => Effect.void,
      orElse: unexpectedStatus
    }))
  ),
    "setSessionAccess": (id, options) => HttpClientRequest.put(`/api/v1/session/${id}/access`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(SetSessionAccess200),
      "400": decodeError("SetSessionAccess400", SetSessionAccess400),
      "401": decodeError("SetSessionAccess401", SetSessionAccess401),
      "404": decodeError("SetSessionAccess404", SetSessionAccess404),
      orElse: unexpectedStatus
    }))
  ),
    "setSessionAudio": (id, options) => HttpClientRequest.put(`/api/v1/session/${id}/audio`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "401": decodeError("SetSessionAudio401", SetSessionAudio401),
      "404": decodeError("SetSessionAudio404", SetSessionAudio404),
      "204": () => Effect.void,
      orElse: unexpectedStatus
    }))
  ),
    "requestSessionIdr": (id, options) => HttpClientRequest.post(`/api/v1/session/${id}/idr`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "401": decodeError("RequestSessionIdr401", RequestSessionIdr401),
      "404": decodeError("RequestSessionIdr404", RequestSessionIdr404),
      "202": () => Effect.void,
      orElse: unexpectedStatus
    }))
  ),
    "streamSessionPads": (id, options) => HttpClientRequest.get(`/api/v1/session/${id}/pads`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "401": decodeError("StreamSessionPads401", StreamSessionPads401),
      "404": decodeError("StreamSessionPads404", StreamSessionPads404),
      "503": decodeError("StreamSessionPads503", StreamSessionPads503),
      orElse: unexpectedStatus
    }))
  ),
    "streamSessionPadsSse": (id) => HttpClientRequest.get(`/api/v1/session/${id}/pads`).pipe(
      sseRequest(StreamSessionPads200Sse)
    ),
    "setSessionPlayer": (id, options) => HttpClientRequest.put(`/api/v1/session/${id}/player`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(SetSessionPlayer200),
      "400": decodeError("SetSessionPlayer400", SetSessionPlayer400),
      "401": decodeError("SetSessionPlayer401", SetSessionPlayer401),
      "404": decodeError("SetSessionPlayer404", SetSessionPlayer404),
      orElse: unexpectedStatus
    }))
  ),
    "statsCaptureLive": (options) => HttpClientRequest.get(`/api/v1/stats/capture/live`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(StatsCaptureLive200),
      "401": decodeError("StatsCaptureLive401", StatsCaptureLive401),
      "404": decodeError("StatsCaptureLive404", StatsCaptureLive404),
      orElse: unexpectedStatus
    }))
  ),
    "statsCaptureStart": (options) => HttpClientRequest.post(`/api/v1/stats/capture/start`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(StatsCaptureStart200),
      "401": decodeError("StatsCaptureStart401", StatsCaptureStart401),
      orElse: unexpectedStatus
    }))
  ),
    "statsCaptureStatus": (options) => HttpClientRequest.get(`/api/v1/stats/capture/status`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(StatsCaptureStatus200),
      "401": decodeError("StatsCaptureStatus401", StatsCaptureStatus401),
      orElse: unexpectedStatus
    }))
  ),
    "statsCaptureStop": (options) => HttpClientRequest.post(`/api/v1/stats/capture/stop`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(StatsCaptureStop200),
      "401": decodeError("StatsCaptureStop401", StatsCaptureStop401),
      "500": decodeError("StatsCaptureStop500", StatsCaptureStop500),
      "204": () => Effect.void,
      orElse: unexpectedStatus
    }))
  ),
    "statsRecordingsList": (options) => HttpClientRequest.get(`/api/v1/stats/recordings`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(StatsRecordingsList200),
      "401": decodeError("StatsRecordingsList401", StatsRecordingsList401),
      orElse: unexpectedStatus
    }))
  ),
    "statsRecordingGet": (id, options) => HttpClientRequest.get(`/api/v1/stats/recordings/${id}`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(StatsRecordingGet200),
      "401": decodeError("StatsRecordingGet401", StatsRecordingGet401),
      "404": decodeError("StatsRecordingGet404", StatsRecordingGet404),
      "500": decodeError("StatsRecordingGet500", StatsRecordingGet500),
      orElse: unexpectedStatus
    }))
  ),
    "statsRecordingDelete": (id, options) => HttpClientRequest.delete(`/api/v1/stats/recordings/${id}`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "401": decodeError("StatsRecordingDelete401", StatsRecordingDelete401),
      "404": decodeError("StatsRecordingDelete404", StatsRecordingDelete404),
      "500": decodeError("StatsRecordingDelete500", StatsRecordingDelete500),
      "204": () => Effect.void,
      orElse: unexpectedStatus
    }))
  ),
    "getStatus": (options) => HttpClientRequest.get(`/api/v1/status`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetStatus200),
      "401": decodeError("GetStatus401", GetStatus401),
      orElse: unexpectedStatus
    }))
  ),
    "getPluginCatalog": (options) => HttpClientRequest.get(`/api/v1/store/catalog`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetPluginCatalog200),
      "401": decodeError("GetPluginCatalog401", GetPluginCatalog401),
      "403": decodeError("GetPluginCatalog403", GetPluginCatalog403),
      orElse: unexpectedStatus
    }))
  ),
    "installPlugin": (options) => HttpClientRequest.post(`/api/v1/store/install`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(InstallPlugin202),
      "400": decodeError("InstallPlugin400", InstallPlugin400),
      "401": decodeError("InstallPlugin401", InstallPlugin401),
      "403": decodeError("InstallPlugin403", InstallPlugin403),
      "409": decodeError("InstallPlugin409", InstallPlugin409),
      orElse: unexpectedStatus
    }))
  ),
    "listInstalledPlugins": (options) => HttpClientRequest.get(`/api/v1/store/installed`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(ListInstalledPlugins200),
      "401": decodeError("ListInstalledPlugins401", ListInstalledPlugins401),
      "403": decodeError("ListInstalledPlugins403", ListInstalledPlugins403),
      orElse: unexpectedStatus
    }))
  ),
    "listPluginJobs": (options) => HttpClientRequest.get(`/api/v1/store/jobs`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(ListPluginJobs200),
      "401": decodeError("ListPluginJobs401", ListPluginJobs401),
      "403": decodeError("ListPluginJobs403", ListPluginJobs403),
      orElse: unexpectedStatus
    }))
  ),
    "getPluginJob": (id, options) => HttpClientRequest.get(`/api/v1/store/jobs/${id}`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetPluginJob200),
      "401": decodeError("GetPluginJob401", GetPluginJob401),
      "403": decodeError("GetPluginJob403", GetPluginJob403),
      "404": decodeError("GetPluginJob404", GetPluginJob404),
      orElse: unexpectedStatus
    }))
  ),
    "refreshPluginCatalog": (options) => HttpClientRequest.post(`/api/v1/store/refresh`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(RefreshPluginCatalog200),
      "401": decodeError("RefreshPluginCatalog401", RefreshPluginCatalog401),
      "403": decodeError("RefreshPluginCatalog403", RefreshPluginCatalog403),
      orElse: unexpectedStatus
    }))
  ),
    "getPluginRuntime": (options) => HttpClientRequest.get(`/api/v1/store/runtime`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetPluginRuntime200),
      "401": decodeError("GetPluginRuntime401", GetPluginRuntime401),
      "403": decodeError("GetPluginRuntime403", GetPluginRuntime403),
      orElse: unexpectedStatus
    }))
  ),
    "setPluginRuntime": (options) => HttpClientRequest.post(`/api/v1/store/runtime`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(SetPluginRuntime200),
      "400": decodeError("SetPluginRuntime400", SetPluginRuntime400),
      "401": decodeError("SetPluginRuntime401", SetPluginRuntime401),
      "403": decodeError("SetPluginRuntime403", SetPluginRuntime403),
      orElse: unexpectedStatus
    }))
  ),
    "listPluginSources": (options) => HttpClientRequest.get(`/api/v1/store/sources`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(ListPluginSources200),
      "401": decodeError("ListPluginSources401", ListPluginSources401),
      "403": decodeError("ListPluginSources403", ListPluginSources403),
      orElse: unexpectedStatus
    }))
  ),
    "putPluginSource": (name, options) => HttpClientRequest.put(`/api/v1/store/sources/${name}`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "400": decodeError("PutPluginSource400", PutPluginSource400),
      "401": decodeError("PutPluginSource401", PutPluginSource401),
      "403": decodeError("PutPluginSource403", PutPluginSource403),
      "204": () => Effect.void,
      orElse: unexpectedStatus
    }))
  ),
    "deletePluginSource": (name, options) => HttpClientRequest.delete(`/api/v1/store/sources/${name}`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "401": decodeError("DeletePluginSource401", DeletePluginSource401),
      "403": decodeError("DeletePluginSource403", DeletePluginSource403),
      "204": () => Effect.void,
      orElse: unexpectedStatus
    }))
  ),
    "uninstallPlugin": (options) => HttpClientRequest.post(`/api/v1/store/uninstall`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(UninstallPlugin202),
      "400": decodeError("UninstallPlugin400", UninstallPlugin400),
      "401": decodeError("UninstallPlugin401", UninstallPlugin401),
      "403": decodeError("UninstallPlugin403", UninstallPlugin403),
      "409": decodeError("UninstallPlugin409", UninstallPlugin409),
      orElse: unexpectedStatus
    }))
  ),
    "applyUpdate": (options) => HttpClientRequest.post(`/api/v1/update/apply`).pipe(
    HttpClientRequest.bodyJsonUnsafe(options.payload),
    withResponse(options.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(ApplyUpdate202),
      "401": decodeError("ApplyUpdate401", ApplyUpdate401),
      "409": decodeError("ApplyUpdate409", ApplyUpdate409),
      orElse: unexpectedStatus
    }))
  ),
    "forceUpdateCheck": (options) => HttpClientRequest.post(`/api/v1/update/check`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(ForceUpdateCheck200),
      "401": decodeError("ForceUpdateCheck401", ForceUpdateCheck401),
      "409": decodeError("ForceUpdateCheck409", ForceUpdateCheck409),
      "429": decodeError("ForceUpdateCheck429", ForceUpdateCheck429),
      orElse: unexpectedStatus
    }))
  ),
    "getUpdateStatus": (options) => HttpClientRequest.get(`/api/v1/update/status`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetUpdateStatus200),
      "401": decodeError("GetUpdateStatus401", GetUpdateStatus401),
      orElse: unexpectedStatus
    }))
  ),
    "getWebTransport": (options) => HttpClientRequest.get(`/api/v1/webtransport`).pipe(
    withResponse(options?.config)(HttpClientResponse.matchStatus({
      "2xx": decodeSuccess(GetWebTransport200),
      "404": decodeError("GetWebTransport404", GetWebTransport404),
      orElse: unexpectedStatus
    }))
  )
  }
}

export interface Punktfunk {
  readonly httpClient: HttpClient.HttpClient
  /**
* Per-caller view: platform availability plus whether this caller may invoke each one.
* Admin: everything permitted. Paired cert: power per the device's live Host-power grant,
* `display.next` while the device owns a live session. Unknown ids still render with the
* server-supplied title.
*/
readonly "listActions": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof ListActions200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"ListActions401", typeof ListActions401.Type>>
  /**
* Id-only, empty body: nothing in the request reaches the privileged path. Power actions
* answer `202`, end every session (typed HostPower close), wait ~1 s so this response
* flushes, then act; paired-cert callers need Host power and are `409` while another
* device's session is live, the admin console never is. One power action at a time.
* `display.next` answers `200` with the monitor now streamed and ends nothing; a paired
* cert needs a live session of its own.
*/
readonly "invokeAction": <Config extends OperationConfig>(id: string, options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof InvokeAction200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"InvokeAction401", typeof InvokeAction401.Type> | PunktfunkError<"InvokeAction403", typeof InvokeAction403.Type> | PunktfunkError<"InvokeAction404", typeof InvokeAction404.Type> | PunktfunkError<"InvokeAction409", typeof InvokeAction409.Type> | PunktfunkError<"InvokeAction500", typeof InvokeAction500.Type> | PunktfunkError<"InvokeAction501", typeof InvokeAction501.Type>>
  /**
* Start a device exchange
*/
readonly "postDeviceChallenge": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof PostDeviceChallenge200.Type, Config>, HttpClientError.HttpClientError | SchemaError>
  /**
* Exchange a device signature for a token
*/
readonly "postDeviceToken": <Config extends OperationConfig>(options: { readonly payload: typeof PostDeviceTokenRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof PostDeviceToken200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"PostDeviceToken401", typeof PostDeviceToken401.Type>>
  /**
* List uploaded client log bundles
*/
readonly "clientLogsList": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof ClientLogsList200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"ClientLogsList401", typeof ClientLogsList401.Type>>
  /**
* A paired device posts plain text under its streaming cert — no bearer. Cap 1 MiB, newest
* few per device kept. Write-only: uploading grants no read.
*/
readonly "clientLogsUpload": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof ClientLogsUpload201.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"ClientLogsUpload400", typeof ClientLogsUpload400.Type> | PunktfunkError<"ClientLogsUpload403", typeof ClientLogsUpload403.Type> | PunktfunkError<"ClientLogsUpload413", typeof ClientLogsUpload413.Type> | PunktfunkError<"ClientLogsUpload422", typeof ClientLogsUpload422.Type> | PunktfunkError<"ClientLogsUpload500", typeof ClientLogsUpload500.Type>>
  /**
* Download a client log bundle
*/
readonly "clientLogsGet": <Config extends OperationConfig>(id: string, options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<void, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"ClientLogsGet401", typeof ClientLogsGet401.Type> | PunktfunkError<"ClientLogsGet404", typeof ClientLogsGet404.Type> | PunktfunkError<"ClientLogsGet500", typeof ClientLogsGet500.Type>>
  /**
* `404` if there is no such bundle.
*/
readonly "clientLogsDelete": <Config extends OperationConfig>(id: string, options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<void, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"ClientLogsDelete401", typeof ClientLogsDelete401.Type> | PunktfunkError<"ClientLogsDelete404", typeof ClientLogsDelete404.Type> | PunktfunkError<"ClientLogsDelete500", typeof ClientLogsDelete500.Type>>
  /**
* List paired clients
*/
readonly "listPairedClients": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof ListPairedClients200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"ListPairedClients401", typeof ListPairedClients401.Type>>
  /**
* Collection form of [`unpair_client`]: one persisted write, same revocation across the
* set. Idempotent, so 200 rather than 204/404 — an already-empty store still satisfies
* "unpair everything", and the body says whether that was three devices or none.
*/
readonly "unpairAllClients": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof UnpairAllClients200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"UnpairAllClients401", typeof UnpairAllClients401.Type>>
  /**
* Persisted. A live GameStream session owned by this certificate is ended
* (TERMINATION+disconnect). Removing the last pairing closes the ENet control port.
* nvhttp TLS still completes a handshake with any well-formed client cert — authorization
* is per-request via the paired-fingerprint check.
*/
readonly "unpairClient": <Config extends OperationConfig>(fingerprint: string, options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<void, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"UnpairClient400", typeof UnpairClient400.Type> | PunktfunkError<"UnpairClient401", typeof UnpairClient401.Type> | PunktfunkError<"UnpairClient404", typeof UnpairClient404.Type>>
  /**
* Cosmetic: no certificate, no trust change. Stored beside the pairing store; unpairing
* the device forgets it.
*/
readonly "renameClient": <Config extends OperationConfig>(fingerprint: string, options: { readonly payload: typeof RenameClientRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof RenameClient200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"RenameClient400", typeof RenameClient400.Type> | PunktfunkError<"RenameClient401", typeof RenameClient401.Type> | PunktfunkError<"RenameClient404", typeof RenameClient404.Type>>
  /**
* Each row carries availability and whether `Auto` resolves to it. Clients pass
* `id` to `--compositor` or `PUNKTFUNK_COMPOSITOR_*`.
*/
readonly "listCompositors": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof ListCompositors200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"ListCompositors401", typeof ListCompositors401.Type>>
  /**
* Probes run at startup and on `POST /diagnostics/refresh`.
*/
readonly "getDiagnostics": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetDiagnostics200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetDiagnostics401", typeof GetDiagnostics401.Type>>
  /**
* Returns the new report. Poll GET; membership and udev rules only change
* after the operator changes them.
*/
readonly "refreshDiagnostics": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof RefreshDiagnostics200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"RefreshDiagnostics401", typeof RefreshDiagnostics401.Type>>
  /**
* Absent fields follow the host policy; an unknown device answers with an empty
* overlay rather than 404 — "follows host" is a real answer, not a missing one.
*/
readonly "getDisplayClient": <Config extends OperationConfig>(fingerprint: string, options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetDisplayClient200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetDisplayClient401", typeof GetDisplayClient401.Type>>
  /**
* The WHOLE overlay: a field absent from the body stops being pinned and the
* device follows the host again. An overlay that pins nothing is dropped, which
* is the same as `DELETE`.
*/
readonly "setDisplayClient": <Config extends OperationConfig>(fingerprint: string, options: { readonly payload: typeof SetDisplayClientRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof SetDisplayClient200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"SetDisplayClient400", typeof SetDisplayClient400.Type> | PunktfunkError<"SetDisplayClient401", typeof SetDisplayClient401.Type> | PunktfunkError<"SetDisplayClient500", typeof SetDisplayClient500.Type>>
  /**
* Make one device follow the host again
*/
readonly "deleteDisplayClient": <Config extends OperationConfig>(fingerprint: string, options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof DeleteDisplayClient200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"DeleteDisplayClient401", typeof DeleteDisplayClient401.Type> | PunktfunkError<"DeleteDisplayClient500", typeof DeleteDisplayClient500.Type>>
  /**
* Persist per-identity-slot `(x, y)` offsets and switch the layout to manual. Applies on the next
* connect (a live group re-applies on its next acquire). Locks current effective behavior into
* explicit fields so arranging never silently changes keep-alive/topology/conflict/identity.
* See `design/display-management.md`.
*/
readonly "setDisplayLayout": <Config extends OperationConfig>(options: { readonly payload: typeof SetDisplayLayoutRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof SetDisplayLayout200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"SetDisplayLayout401", typeof SetDisplayLayout401.Type> | PunktfunkError<"SetDisplayLayout500", typeof SetDisplayLayout500.Type>>
  /**
* Heads this host has, for a capture-pin picker. Read-only; does not create, move, or disable.
* Managed virtual displays are `/display/state`. See `design/per-monitor-portal-capture.md`.
*/
readonly "getDisplayMonitors": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetDisplayMonitors200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetDisplayMonitors401", typeof GetDisplayMonitors401.Type>>
  /**
* Named field-bundles in `display-presets.json`. Also on `GET /display/settings` as `custom_presets`.
*/
readonly "listCustomPresets": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof ListCustomPresets200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"ListCustomPresets401", typeof ListCustomPresets401.Type>>
  /**
* Named bundle of the display-behavior axes. Host assigns a stable id in the body. Apply with
* `PUT /display/settings` carrying a `Custom` policy of its `fields` — no separate apply route.
*/
readonly "createCustomPreset": <Config extends OperationConfig>(options: { readonly payload: typeof CreateCustomPresetRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof CreateCustomPreset201.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"CreateCustomPreset400", typeof CreateCustomPreset400.Type> | PunktfunkError<"CreateCustomPreset401", typeof CreateCustomPreset401.Type> | PunktfunkError<"CreateCustomPreset500", typeof CreateCustomPreset500.Type>>
  /**
* Update a custom preset
*/
readonly "updateCustomPreset": <Config extends OperationConfig>(id: string, options: { readonly payload: typeof UpdateCustomPresetRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof UpdateCustomPreset200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"UpdateCustomPreset400", typeof UpdateCustomPreset400.Type> | PunktfunkError<"UpdateCustomPreset401", typeof UpdateCustomPreset401.Type> | PunktfunkError<"UpdateCustomPreset404", typeof UpdateCustomPreset404.Type> | PunktfunkError<"UpdateCustomPreset500", typeof UpdateCustomPreset500.Type>>
  /**
* Removes it from the catalog. The active policy is untouched — catalog and
* `display-settings.json` are decoupled.
*/
readonly "deleteCustomPreset": <Config extends OperationConfig>(id: string, options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<void, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"DeleteCustomPreset401", typeof DeleteCustomPreset401.Type> | PunktfunkError<"DeleteCustomPreset404", typeof DeleteCustomPreset404.Type> | PunktfunkError<"DeleteCustomPreset500", typeof DeleteCustomPreset500.Type>>
  /**
* Tear down lingering/pinned displays now. `slot` releases one; omit to release all.
* Active (streaming) displays are never torn down here — that is session control.
*/
readonly "releaseDisplay": <Config extends OperationConfig>(options: { readonly payload: typeof ReleaseDisplayRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof ReleaseDisplay200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"ReleaseDisplay401", typeof ReleaseDisplay401.Type>>
  /**
* Stored policy, preset expansions, and which options this build enforces.
* See `design/display-management.md`.
*/
readonly "getDisplaySettings": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetDisplaySettings200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetDisplaySettings401", typeof GetDisplaySettings401.Type>>
  /**
* Persists (validated + clamped). Applies on the next connect/teardown; a running session keeps
* the display it opened on. `keep_alive: forever` pins until `POST /display/release`.
*/
readonly "setDisplaySettings": <Config extends OperationConfig>(options: { readonly payload: typeof SetDisplaySettingsRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof SetDisplaySettings200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"SetDisplaySettings400", typeof SetDisplaySettings400.Type> | PunktfunkError<"SetDisplaySettings401", typeof SetDisplaySettings401.Type> | PunktfunkError<"SetDisplaySettings500", typeof SetDisplaySettings500.Type>>
  /**
* Active (streaming), lingering (countdown to teardown), or pinned.
* See `design/display-management.md`.
*/
readonly "getDisplayState": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetDisplayState200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetDisplayState401", typeof GetDisplayState401.Type>>
  /**
* `id:` is `seq`, `event:` is kind, `data:` is HostEvent JSON. Resume with `Last-Event-ID`
* or `?since=`; `event: dropped` means the ring no longer has that cursor — resync from REST.
* `event: live` follows the catch-up; frames after it happened after you connected.
*/
readonly "streamEvents": <Config extends OperationConfig>(options: { readonly params?: typeof StreamEventsParams.Encoded | undefined; readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<void, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"StreamEvents401", typeof StreamEvents401.Type> | PunktfunkError<"StreamEvents503", typeof StreamEvents503.Type>>
  /**
* `id:` is `seq`, `event:` is kind, `data:` is HostEvent JSON. Resume with `Last-Event-ID`
* or `?since=`; `event: dropped` means the ring no longer has that cursor — resync from REST.
* `event: live` follows the catch-up; frames after it happened after you connected.
*/
readonly "streamEventsSse": (options: { readonly params?: typeof StreamEventsParams.Encoded | undefined } | undefined) => Stream.Stream<{ readonly event: string; readonly id: string | undefined; readonly data: typeof StreamEvents200Sse.Type }, HttpClientError.HttpClientError | SchemaError | Sse.Retry, typeof StreamEvents200Sse.DecodingServices>
  /**
* Ends games waiting out the reconnect window. With `streaming` and an
* `app_id`, also ends that title where it is still on a live session — the
* move a player has after a launch that never produced a game. The session
* itself stays up (`DELETE /session` plus `game_on_session_end`).
*/
readonly "endGame": <Config extends OperationConfig>(options: { readonly payload: typeof EndGameRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof EndGame200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"EndGame401", typeof EndGame401.Type> | PunktfunkError<"EndGame409", typeof EndGame409.Type>>
  /**
* Preference changes apply to the next session; a running session keeps its GPU.
*/
readonly "listGpus": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof ListGpus200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"ListGpus401", typeof ListGpus401.Type>>
  /**
* `auto` = env pin else max VRAM; `manual` pins capture+encode. Applies to the next session.
* An absent preferred GPU falls back to auto rather than failing the session.
*/
readonly "setGpuPreference": <Config extends OperationConfig>(options: { readonly payload: typeof SetGpuPreferenceRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof SetGpuPreference200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"SetGpuPreference400", typeof SetGpuPreference400.Type> | PunktfunkError<"SetGpuPreference401", typeof SetGpuPreference401.Type> | PunktfunkError<"SetGpuPreference500", typeof SetGpuPreference500.Type>>
  /**
* Unauthenticated: `require_auth` exempts it.
*/
readonly "getHealth": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetHealth200.Type, Config>, HttpClientError.HttpClientError | SchemaError>
  /**
* Empty document when none is stored.
*/
readonly "getHooks": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetHooks200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetHooks401", typeof GetHooks401.Type>>
  /**
* Whole-document PUT, not a patch. Applies from the next event. Commands run as the host
* user (the interactive session on Windows).
*/
readonly "setHooks": <Config extends OperationConfig>(options: { readonly payload: typeof SetHooksRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof SetHooks200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"SetHooks400", typeof SetHooks400.Type> | PunktfunkError<"SetHooks401", typeof SetHooks401.Type> | PunktfunkError<"SetHooks500", typeof SetHooks500.Type>>
  /**
* Host identity and capabilities
*/
readonly "getHostInfo": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetHostInfo200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetHostInfo401", typeof GetHostInfo401.Type>>
  /**
* Audio output streams on the host right now, by app. Empty on a host that cannot list them.
*/
readonly "getPlayingApps": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetPlayingApps200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetPlayingApps401", typeof GetPlayingApps401.Type>>
  /**
* Every setting this host acts on, with the value in force and what set it.
*/
readonly "getHostSettings": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetHostSettings200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetHostSettings401", typeof GetHostSettings401.Type>>
  /**
* Partial: only the named settings change, and `null` resets one. Every value is checked
* before anything is written. Applies per setting's `apply`: the next session, or a restart.
*/
readonly "patchHostSettings": <Config extends OperationConfig>(options: { readonly payload: typeof PatchHostSettingsRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof PatchHostSettings200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"PatchHostSettings400", typeof PatchHostSettings400.Type> | PunktfunkError<"PatchHostSettings401", typeof PatchHostSettings401.Type> | PunktfunkError<"PatchHostSettings500", typeof PatchHostSettings500.Type>>
  /**
* The mode and accent colour of the desktop this host runs on, for a console that follows it.
* Every field is nullable: "no answer" is a real state, and the console renders it as
* "none detected" rather than guessing.
*/
readonly "getHostTheme": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetHostTheme200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetHostTheme401", typeof GetHostTheme401.Type>>
  /**
* Plugin-synced entries plus custom ones. Art the host can serve is rewritten to this API's
* art proxy, local paths and remote URLs alike; a URL the proxy already refused passes
* through. `?provider=` / `?platform=` (case-insensitive) narrow.
* 
* The operator lane sees hidden titles (`hidden: true`) so the console can un-hide them.
* Every other lane is filtered upstream and cannot tell they exist.
*/
readonly "getLibrary": <Config extends OperationConfig>(options: { readonly params?: typeof GetLibraryParams.Encoded | undefined; readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetLibrary200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetLibrary401", typeof GetLibrary401.Type>>
  /**
* Resolves `kind` (`portrait` | `hero` | `logo` | `header`) for a catalog id and returns the
* bytes: a launcher's cover cache on the host disk, or a remote URL the host fetches once on
* the first miss and then serves from its own store. Unknown id or kind is 404 so the client
* can try the next candidate, and so is a URL the fetch refused — `GET /library` advertises
* that one verbatim again. The response carries `Cache-Control` and an `ETag` of the bytes; a
* request whose `If-None-Match` names that tag gets 304 with no body.
*/
readonly "getLibraryArt": <Config extends OperationConfig>(id: string, kind: string, options: { readonly params?: typeof GetLibraryArtParams.Encoded | undefined; readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<void, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetLibraryArt401", typeof GetLibraryArt401.Type> | PunktfunkError<"GetLibraryArt404", typeof GetLibraryArt404.Type>>
  /**
* A user-curated entry. The host assigns a stable id, returned in the body.
*/
readonly "createCustomGame": <Config extends OperationConfig>(options: { readonly payload: typeof CreateCustomGameRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof CreateCustomGame201.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"CreateCustomGame400", typeof CreateCustomGame400.Type> | PunktfunkError<"CreateCustomGame401", typeof CreateCustomGame401.Type> | PunktfunkError<"CreateCustomGame500", typeof CreateCustomGame500.Type>>
  /**
* Operator lane only: the hint names host paths, which is why the catalog read
* model leaves it out. The paired-cert allowlist names `GET /library` exactly.
*/
readonly "getCustomGame": <Config extends OperationConfig>(id: string, options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetCustomGame200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetCustomGame401", typeof GetCustomGame401.Type> | PunktfunkError<"GetCustomGame404", typeof GetCustomGame404.Type>>
  readonly "updateCustomGame": <Config extends OperationConfig>(id: string, options: { readonly payload: typeof UpdateCustomGameRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof UpdateCustomGame200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"UpdateCustomGame400", typeof UpdateCustomGame400.Type> | PunktfunkError<"UpdateCustomGame401", typeof UpdateCustomGame401.Type> | PunktfunkError<"UpdateCustomGame404", typeof UpdateCustomGame404.Type> | PunktfunkError<"UpdateCustomGame500", typeof UpdateCustomGame500.Type>>
  readonly "deleteCustomGame": <Config extends OperationConfig>(id: string, options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<void, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"DeleteCustomGame401", typeof DeleteCustomGame401.Type> | PunktfunkError<"DeleteCustomGame404", typeof DeleteCustomGame404.Type> | PunktfunkError<"DeleteCustomGame500", typeof DeleteCustomGame500.Type>>
  /**
* Curation, not access control: the title leaves every play surface and launch resolution
* but is not deleted. The operator console still lists it (`hidden: true`) so it can return.
* The id is not required to exist now — a plugin mid-sync or an unmounted drive would
* otherwise refuse a choice that should stick. Emits `library.changed` only on a real change.
*/
readonly "setLibraryEntryHidden": <Config extends OperationConfig>(id: string, options: { readonly payload: typeof SetLibraryEntryHiddenRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof SetLibraryEntryHidden200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"SetLibraryEntryHidden400", typeof SetLibraryEntryHidden400.Type> | PunktfunkError<"SetLibraryEntryHidden401", typeof SetLibraryEntryHidden401.Type> | PunktfunkError<"SetLibraryEntryHidden500", typeof SetLibraryEntryHidden500.Type>>
  /**
* Every source that has pushed a result, in the operator's order, with its switches and how
* many entries it has something for.
*/
readonly "listLibraryMetadata": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof ListLibraryMetadata200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"ListLibraryMetadata401", typeof ListLibraryMetadata401.Type>>
  /**
* The array is the new order; each row sets `enabled` and `replace` ("Use for every game",
* art only). A source the array leaves out keeps its switches and goes after the named ones.
* Emits `library.changed`.
*/
readonly "setLibraryMetadata": <Config extends OperationConfig>(options: { readonly payload: typeof SetLibraryMetadataRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof SetLibraryMetadata200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"SetLibraryMetadata401", typeof SetLibraryMetadata401.Type> | PunktfunkError<"SetLibraryMetadata500", typeof SetLibraryMetadata500.Type>>
  /**
* Everything the source has, keyed by library id: art for the four slots and `GameMeta`
* fields. The host merges it into `GET /library` at read time; a source fills only what the
* entry lacks unless the operator set it to replace art. A value the host does not store is
* dropped, not refused. A new source takes its place in the order by `matching`, exact first.
* Emits `library.changed` with the source as `source` when anything changed.
*/
readonly "putLibraryMetadata": <Config extends OperationConfig>(source: string, options: { readonly payload: typeof PutLibraryMetadataRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof PutLibraryMetadata200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"PutLibraryMetadata400", typeof PutLibraryMetadata400.Type> | PunktfunkError<"PutLibraryMetadata401", typeof PutLibraryMetadata401.Type> | PunktfunkError<"PutLibraryMetadata403", typeof PutLibraryMetadata403.Type> | PunktfunkError<"PutLibraryMetadata500", typeof PutLibraryMetadata500.Type>>
  /**
* Its result and its place in the order, for plugin uninstall. Emits `library.changed`
* when there was anything to forget.
*/
readonly "deleteLibraryMetadata": <Config extends OperationConfig>(source: string, options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof DeleteLibraryMetadata200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"DeleteLibraryMetadata400", typeof DeleteLibraryMetadata400.Type> | PunktfunkError<"DeleteLibraryMetadata401", typeof DeleteLibraryMetadata401.Type> | PunktfunkError<"DeleteLibraryMetadata403", typeof DeleteLibraryMetadata403.Type> | PunktfunkError<"DeleteLibraryMetadata500", typeof DeleteLibraryMetadata500.Type>>
  /**
* The operator's choice beats the entry's own art and every metadata source, and survives
* the provider's next reconcile. `url: null` clears the pick. The id is not required to
* exist now, as with hiding. Emits `library.changed`.
*/
readonly "setLibraryArtPick": <Config extends OperationConfig>(id: string, options: { readonly payload: typeof SetLibraryArtPickRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof SetLibraryArtPick200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"SetLibraryArtPick400", typeof SetLibraryArtPick400.Type> | PunktfunkError<"SetLibraryArtPick401", typeof SetLibraryArtPick401.Type> | PunktfunkError<"SetLibraryArtPick500", typeof SetLibraryArtPick500.Type>>
  /**
* The payload is the desired set, keyed by `external_id`. The host diffs, keeps surviving
* host ids stable, and drops orphans. Empty array removes everything this provider owns.
* Emits `library.changed` with the provider as `source`.
* 
* `?store=` claims that store: entries surface as `<store>:<external_id>` with the store
* badge. One provider per store; a second claimant gets 409. Release the claim with
* `DELETE`, not an empty reconcile — a store can have zero installed titles.
*/
readonly "reconcileProviderEntries": <Config extends OperationConfig>(provider: string, options: { readonly params?: typeof ReconcileProviderEntriesParams.Encoded | undefined; readonly payload: typeof ReconcileProviderEntriesRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof ReconcileProviderEntries200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"ReconcileProviderEntries400", typeof ReconcileProviderEntries400.Type> | PunktfunkError<"ReconcileProviderEntries401", typeof ReconcileProviderEntries401.Type> | PunktfunkError<"ReconcileProviderEntries409", typeof ReconcileProviderEntries409.Type> | PunktfunkError<"ReconcileProviderEntries500", typeof ReconcileProviderEntries500.Type>>
  /**
* Everything owned by `{provider}`, for plugin uninstall. Emits `library.changed`
* when anything was removed.
*/
readonly "deleteProviderEntries": <Config extends OperationConfig>(provider: string, options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof DeleteProviderEntries200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"DeleteProviderEntries400", typeof DeleteProviderEntries400.Type> | PunktfunkError<"DeleteProviderEntries401", typeof DeleteProviderEntries401.Type> | PunktfunkError<"DeleteProviderEntries500", typeof DeleteProviderEntries500.Type>>
  /**
* Live counterpart to reconcile `detect` hints: the body is the complete running set
* ([`crate::runstate`]). Missed events and plugin restarts self-correct on the next PUT.
* 
* Authoritative for [`crate::runstate::REPORT_TTL`] unless restated; a dead plugin then
* falls back to process scanning. Re-report on change and on a timer inside the window.
* Titles the provider does not currently publish count as `unknown`, not an error — a
* report may race its own reconcile.
*/
readonly "reportProviderRunning": <Config extends OperationConfig>(provider: string, options: { readonly payload: typeof ReportProviderRunningRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof ReportProviderRunning200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"ReportProviderRunning400", typeof ReportProviderRunning400.Type> | PunktfunkError<"ReportProviderRunning401", typeof ReportProviderRunning401.Type> | PunktfunkError<"ReportProviderRunning403", typeof ReportProviderRunning403.Type>>
  /**
* One row per installed library plugin, with its enable state. Sources default to
* enabled; disabling hides titles from the next read. The custom store is not a
* source and is always on. Every row is
* `origin: "plugin"`.
*/
readonly "listLibraryScanners": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof ListLibraryScanners200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"ListLibraryScanners401", typeof ListLibraryScanners401.Type>>
  /**
* Takes effect on the next library read. Disabling hides titles; the plugin may keep
* reconciling while off. Nothing is deleted. Emits `library.changed` when the state changes.
*/
readonly "setLibraryScanner": <Config extends OperationConfig>(id: string, options: { readonly payload: typeof SetLibraryScannerRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof SetLibraryScanner200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"SetLibraryScanner401", typeof SetLibraryScanner401.Type> | PunktfunkError<"SetLibraryScanner403", typeof SetLibraryScanner403.Type> | PunktfunkError<"SetLibraryScanner404", typeof SetLibraryScanner404.Type> | PunktfunkError<"SetLibraryScanner500", typeof SetLibraryScanner500.Type>>
  /**
* Unauthenticated; `require_auth` admits loopback only.
*/
readonly "getLocalSummary": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetLocalSummary200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetLocalSummary401", typeof GetLocalSummary401.Type>>
  /**
* In-memory, DEBUG and above, independent of `RUST_LOG`. Poll with `after` = last
* `next`. `dropped: true` means the ring wrapped between polls and entries were
* evicted.
*/
readonly "logsGet": <Config extends OperationConfig>(options: { readonly params?: typeof LogsGetParams.Encoded | undefined; readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof LogsGet200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"LogsGet401", typeof LogsGet401.Type>>
  /**
* List native paired clients
*/
readonly "listNativeClients": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof ListNativeClients200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"ListNativeClients401", typeof ListNativeClients401.Type>>
  /**
* One persisted write, not a loop (a mid-loop failure would half-empty the
* store), and every live native session those clients own is stopped.
* Idempotent, so 200 rather than the single unpair's 204/404: an empty store
* still succeeds, and `unpaired` tells the operator what it meant.
*/
readonly "unpairAllNativeClients": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof UnpairAllNativeClients200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"UnpairAllNativeClients401", typeof UnpairAllNativeClients401.Type> | PunktfunkError<"UnpairAllNativeClients500", typeof UnpairAllNativeClients500.Type> | PunktfunkError<"UnpairAllNativeClients503", typeof UnpairAllNativeClients503.Type>>
  /**
* Unpair a native client
*/
readonly "unpairNativeClient": <Config extends OperationConfig>(fingerprint: string, options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<void, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"UnpairNativeClient401", typeof UnpairNativeClient401.Type> | PunktfunkError<"UnpairNativeClient404", typeof UnpairNativeClient404.Type> | PunktfunkError<"UnpairNativeClient503", typeof UnpairNativeClient503.Type>>
  /**
* Partial edit of grants/expiry. Omitted fields keep their current value;
* live sessions pick it up immediately. 404 if the fingerprint is not paired
* — this is not a pair path.
*/
readonly "updateNativeClientAccess": <Config extends OperationConfig>(fingerprint: string, options: { readonly payload: typeof UpdateNativeClientAccessRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof UpdateNativeClientAccess200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"UpdateNativeClientAccess400", typeof UpdateNativeClientAccess400.Type> | PunktfunkError<"UpdateNativeClientAccess401", typeof UpdateNativeClientAccess401.Type> | PunktfunkError<"UpdateNativeClientAccess404", typeof UpdateNativeClientAccess404.Type> | PunktfunkError<"UpdateNativeClientAccess500", typeof UpdateNativeClientAccess500.Type> | PunktfunkError<"UpdateNativeClientAccess503", typeof UpdateNativeClientAccess503.Type>>
  /**
* Poll while armed to show the PIN and countdown. `enabled: false` means
* GameStream only (no `--native`).
*/
readonly "getNativePairing": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetNativePairing200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetNativePairing401", typeof GetNativePairing401.Type>>
  /**
* Disarm native pairing
*/
readonly "disarmNativePairing": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<void, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"DisarmNativePairing401", typeof DisarmNativePairing401.Type> | PunktfunkError<"DisarmNativePairing503", typeof DisarmNativePairing503.Type>>
  /**
* Opens a window and mints a PIN. `grants` / `expires_in_secs` apply to
* whichever device completes the ceremony.
*/
readonly "armNativePairing": <Config extends OperationConfig>(options: { readonly payload: typeof ArmNativePairingRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof ArmNativePairing200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"ArmNativePairing400", typeof ArmNativePairing400.Type> | PunktfunkError<"ArmNativePairing401", typeof ArmNativePairing401.Type> | PunktfunkError<"ArmNativePairing503", typeof ArmNativePairing503.Type>>
  /**
* Unpaired knocks while pairing is required. Approve to pair without a PIN.
* Entries expire after ~10 minutes.
*/
readonly "listPendingDevices": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof ListPendingDevices200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"ListPendingDevices401", typeof ListPendingDevices401.Type>>
  /**
* Pairs the fingerprint immediately (no PIN). `{}` keeps the knock name and
* stored access (full/permanent on first pairing). The response is the stored
* record, not necessarily this request's inputs.
*/
readonly "approvePendingDevice": <Config extends OperationConfig>(id: string, options: { readonly payload: typeof ApprovePendingDeviceRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof ApprovePendingDevice200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"ApprovePendingDevice400", typeof ApprovePendingDevice400.Type> | PunktfunkError<"ApprovePendingDevice401", typeof ApprovePendingDevice401.Type> | PunktfunkError<"ApprovePendingDevice404", typeof ApprovePendingDevice404.Type> | PunktfunkError<"ApprovePendingDevice409", typeof ApprovePendingDevice409.Type> | PunktfunkError<"ApprovePendingDevice500", typeof ApprovePendingDevice500.Type> | PunktfunkError<"ApprovePendingDevice503", typeof ApprovePendingDevice503.Type>>
  /**
* Drops the request. Not a blocklist — the next attempt knocks again.
*/
readonly "denyPendingDevice": <Config extends OperationConfig>(id: string, options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<void, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"DenyPendingDevice401", typeof DenyPendingDevice401.Type> | PunktfunkError<"DenyPendingDevice404", typeof DenyPendingDevice404.Type> | PunktfunkError<"DenyPendingDevice503", typeof DenyPendingDevice503.Type>>
  /**
* Poll this to know when to prompt for the PIN Moonlight displays.
*/
readonly "getPairingStatus": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetPairingStatus200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetPairingStatus401", typeof GetPairingStatus401.Type>>
  /**
* Completes the out-of-band half of the handshake.
*/
readonly "submitPairingPin": <Config extends OperationConfig>(options: { readonly payload: typeof SubmitPairingPinRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<void, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"SubmitPairingPin400", typeof SubmitPairingPin400.Type> | PunktfunkError<"SubmitPairingPin401", typeof SubmitPairingPin401.Type> | PunktfunkError<"SubmitPairingPin409", typeof SubmitPairingPin409.Type> | PunktfunkError<"SubmitPairingPin415", typeof SubmitPairingPin415.Type> | PunktfunkError<"SubmitPairingPin422", typeof SubmitPairingPin422.Type>>
  /**
* Admin lane only: grants, pending requests, and denials across all plugins.
*/
readonly "getPluginAccess": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetPluginAccess200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetPluginAccess401", typeof GetPluginAccess401.Type> | PunktfunkError<"GetPluginAccess403", typeof GetPluginAccess403.Type>>
  /**
* Grants, pending requests, and denials for the calling plugin only.
*/
readonly "getPluginAccessRequests": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetPluginAccessRequests200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetPluginAccessRequests401", typeof GetPluginAccessRequests401.Type> | PunktfunkError<"GetPluginAccessRequests403", typeof GetPluginAccessRequests403.Type>>
  /**
* Each path is validated against the real filesystem before it becomes a row the operator
* sees: `granted` (already reachable), `pending`, `denied` (a sticky no), or `refused:<rule>`.
*/
readonly "requestPluginAccess": <Config extends OperationConfig>(options: { readonly payload: typeof RequestPluginAccessRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof RequestPluginAccess200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"RequestPluginAccess401", typeof RequestPluginAccess401.Type> | PunktfunkError<"RequestPluginAccess403", typeof RequestPluginAccess403.Type> | PunktfunkError<"RequestPluginAccess500", typeof RequestPluginAccess500.Type>>
  /**
* `allow` turns a pending request into a grant (the platform ACL lands first, so a failed
* grant stores nothing), or with `write` set grants a path the operator handed over;
* `deny` remembers the no; `forget` removes a grant or denial.
*/
readonly "decidePluginAccess": <Config extends OperationConfig>(plugin: string, options: { readonly payload: typeof DecidePluginAccessRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof DecidePluginAccess200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"DecidePluginAccess400", typeof DecidePluginAccess400.Type> | PunktfunkError<"DecidePluginAccess401", typeof DecidePluginAccess401.Type> | PunktfunkError<"DecidePluginAccess403", typeof DecidePluginAccess403.Type> | PunktfunkError<"DecidePluginAccess404", typeof DecidePluginAccess404.Type> | PunktfunkError<"DecidePluginAccess500", typeof DecidePluginAccess500.Type>>
  /**
* Live, secret-free directory. The console fetches the secret separately, server-side.
*/
readonly "listPlugins": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof ListPlugins200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"ListPlugins401", typeof ListPlugins401.Type>>
  /**
* Plugins are not host children — their output never hits the host `tracing` subscriber. Lines
* share the host ring as `plugin:<source>` so `GET /logs` needs no second cursor.
*/
readonly "ingestPluginLogs": <Config extends OperationConfig>(options: { readonly payload: typeof IngestPluginLogsRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<void, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"IngestPluginLogs400", typeof IngestPluginLogs400.Type> | PunktfunkError<"IngestPluginLogs401", typeof IngestPluginLogs401.Type>>
  /**
* Idempotent lease renew (~30 s). Emits `plugins.changed` only when an operator-visible field
* changed; a pure renewal is silent.
*/
readonly "registerPlugin": <Config extends OperationConfig>(id: string, options: { readonly payload: typeof RegisterPluginRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<void, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"RegisterPlugin400", typeof RegisterPlugin400.Type> | PunktfunkError<"RegisterPlugin401", typeof RegisterPlugin401.Type>>
  /**
* Immediate remove (SDK finalizer on SIGTERM). Emits `plugins.changed` only for a live entry;
* unknown/expired is a silent 204.
*/
readonly "deregisterPlugin": <Config extends OperationConfig>(id: string, options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<void, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"DeregisterPlugin401", typeof DeregisterPlugin401.Type>>
  /**
* Server-side `{port, secret}` for the console proxy. The console BFF denylists this from the
* browser.
*/
readonly "getPluginUiCredential": <Config extends OperationConfig>(id: string, options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetPluginUiCredential200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetPluginUiCredential401", typeof GetPluginUiCredential401.Type> | PunktfunkError<"GetPluginUiCredential404", typeof GetPluginUiCredential404.Type>>
  /**
* A deliberate stop: skip keep-alive linger and apply `game_on_session_end`.
*/
readonly "stopSession": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<void, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"StopSession401", typeof StopSession401.Type>>
  readonly "requestIdr": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<void, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"RequestIdr401", typeof RequestIdr401.Type> | PunktfunkError<"RequestIdr409", typeof RequestIdr409.Type>>
  /**
* What each session came to — mode, codec, bitrate, frames, bring-up, and why it ended
* — newest first, at most the last eight. This is the whole of what the host knows, so
* the answer is what a bug report should carry.
* 
* A host that has not streamed since it started answers with an empty list.
*/
readonly "getRecentSessions": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetRecentSessions200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetRecentSessions401", typeof GetRecentSessions401.Type>>
  readonly "getSessionSettings": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetSessionSettings200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetSessionSettings401", typeof GetSessionSettings401.Type>>
  /**
* Persisted clamped. Takes effect on the next decision, including a session
* already streaming — policy is read at session end, not start.
*/
readonly "setSessionSettings": <Config extends OperationConfig>(options: { readonly payload: typeof SetSessionSettingsRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof SetSessionSettings200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"SetSessionSettings400", typeof SetSessionSettings400.Type> | PunktfunkError<"SetSessionSettings401", typeof SetSessionSettings401.Type> | PunktfunkError<"SetSessionSettings500", typeof SetSessionSettings500.Type>>
  /**
* Same deliberate stop as `DELETE /session`, for the one id. Every other live
* session keeps streaming.
*/
readonly "stopOneSession": <Config extends OperationConfig>(id: string, options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<void, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"StopOneSession401", typeof StopOneSession401.Type> | PunktfunkError<"StopOneSession404", typeof StopOneSession404.Type>>
  /**
* Re-points the LIVE grant set — the mask the input thread already checks every
* event against — so a guest gets the pad, or loses it, without reconnecting.
* The pairing's stored access is untouched, and a later edit to it wins.
* 
* The ceiling is the pairing's own mask: what is asked for is ANDed with it, so this
* route can narrow or restore, never grant a device something it was not paired for.
* The applied mask comes back, which is how a caller sees the clamp.
*/
readonly "setSessionAccess": <Config extends OperationConfig>(id: string, options: { readonly payload: typeof SetSessionAccessRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof SetSessionAccess200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"SetSessionAccess400", typeof SetSessionAccess400.Type> | PunktfunkError<"SetSessionAccess401", typeof SetSessionAccess401.Type> | PunktfunkError<"SetSessionAccess404", typeof SetSessionAccess404.Type>>
  /**
* Drops this session's audio at the wire, nothing else: the capturer and the sink
* stay up, so a session sharing the display keeps hearing. The client is told, so
* its overlay can name the silence instead of the player guessing.
*/
readonly "setSessionAudio": <Config extends OperationConfig>(id: string, options: { readonly payload: typeof SetSessionAudioRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<void, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"SetSessionAudio401", typeof SetSessionAudio401.Type> | PunktfunkError<"SetSessionAudio404", typeof SetSessionAudio404.Type>>
  /**
* Request a keyframe on one session
*/
readonly "requestSessionIdr": <Config extends OperationConfig>(id: string, options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<void, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"RequestSessionIdr401", typeof RequestSessionIdr401.Type> | PunktfunkError<"RequestSessionIdr404", typeof RequestSessionIdr404.Type>>
  /**
* One frame per pad state the host applies — what it injects, not what the client
* says it sent — so a controller question is answered from the host's own hand
* instead of an evdev dump. `data:` is a [`PadFrame`]; `event:` is `pad.state`.
* Attaching replays every live pad, so a button already held draws at once.
* 
* Console lane only: a paired certificate is not bound to a session id, and this
* is the operator's own machine watching its own input.
* 
* Nothing is published while nobody is attached, so a console on another page —
* or none at all — costs the input thread one atomic load per pad event.
*/
readonly "streamSessionPads": <Config extends OperationConfig>(id: string, options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<void, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"StreamSessionPads401", typeof StreamSessionPads401.Type> | PunktfunkError<"StreamSessionPads404", typeof StreamSessionPads404.Type> | PunktfunkError<"StreamSessionPads503", typeof StreamSessionPads503.Type>>
  /**
* One frame per pad state the host applies — what it injects, not what the client
* says it sent — so a controller question is answered from the host's own hand
* instead of an evdev dump. `data:` is a [`PadFrame`]; `event:` is `pad.state`.
* Attaching replays every live pad, so a button already held draws at once.
* 
* Console lane only: a paired certificate is not bound to a session id, and this
* is the operator's own machine watching its own input.
* 
* Nothing is published while nobody is attached, so a console on another page —
* or none at all — costs the input thread one atomic load per pad event.
*/
readonly "streamSessionPadsSse": (id: string) => Stream.Stream<{ readonly event: string; readonly id: string | undefined; readonly data: typeof StreamSessionPads200Sse.Type }, HttpClientError.HttpClientError | SchemaError | Sse.Retry, typeof StreamSessionPads200Sse.DecodingServices>
  /**
* Which controller this session is: slot 0 is Player 1, and a local co-op game
* reads that order. Without a pick the slot is whichever comes free, so the pad
* that moves first takes Player 1 and the order changes on every reconnect.
* 
* The pick is a reservation, not a seizure. A pad already built keeps the slot it
* was created under until it re-plugs; a slot another live session asked for first
* stays theirs and this call answers `reserved: false`. It is remembered against
* the device's pairing, so the same device reconnects as the same player — an
* anonymous session's pick lasts only as long as the session.
*/
readonly "setSessionPlayer": <Config extends OperationConfig>(id: string, options: { readonly payload: typeof SetSessionPlayerRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof SetSessionPlayer200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"SetSessionPlayer400", typeof SetSessionPlayer400.Type> | PunktfunkError<"SetSessionPlayer401", typeof SetSessionPlayer401.Type> | PunktfunkError<"SetSessionPlayer404", typeof SetSessionPlayer404.Type>>
  readonly "statsCaptureLive": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof StatsCaptureLive200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"StatsCaptureLive401", typeof StatsCaptureLive401.Type> | PunktfunkError<"StatsCaptureLive404", typeof StatsCaptureLive404.Type>>
  /**
* Streaming loops emit aggregated samples every 1–2 s into the in-progress
* capture (`GET /stats/capture/live`).
*/
readonly "statsCaptureStart": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof StatsCaptureStart200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"StatsCaptureStart401", typeof StatsCaptureStart401.Type>>
  readonly "statsCaptureStatus": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof StatsCaptureStatus200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"StatsCaptureStatus401", typeof StatsCaptureStatus401.Type>>
  /**
* Disarm and write the capture to disk atomically.
*/
readonly "statsCaptureStop": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof StatsCaptureStop200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"StatsCaptureStop401", typeof StatsCaptureStop401.Type> | PunktfunkError<"StatsCaptureStop500", typeof StatsCaptureStop500.Type>>
  /**
* Summaries only (`meta`, no sample body), newest first.
*/
readonly "statsRecordingsList": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof StatsRecordingsList200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"StatsRecordingsList401", typeof StatsRecordingsList401.Type>>
  readonly "statsRecordingGet": <Config extends OperationConfig>(id: string, options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof StatsRecordingGet200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"StatsRecordingGet401", typeof StatsRecordingGet401.Type> | PunktfunkError<"StatsRecordingGet404", typeof StatsRecordingGet404.Type> | PunktfunkError<"StatsRecordingGet500", typeof StatsRecordingGet500.Type>>
  readonly "statsRecordingDelete": <Config extends OperationConfig>(id: string, options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<void, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"StatsRecordingDelete401", typeof StatsRecordingDelete401.Type> | PunktfunkError<"StatsRecordingDelete404", typeof StatsRecordingDelete404.Type> | PunktfunkError<"StatsRecordingDelete500", typeof StatsRecordingDelete500.Type>>
  /**
* Live host status
*/
readonly "getStatus": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetStatus200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetStatus401", typeof GetStatus401.Type>>
  /**
* Merged view across sources, annotated with what this host has and can run. A source past its
* freshness window is refreshed first; a miss keeps the last copy marked `stale` — the pin
* travelled with the entry.
*/
readonly "getPluginCatalog": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetPluginCatalog200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetPluginCatalog401", typeof GetPluginCatalog401.Type> | PunktfunkError<"GetPluginCatalog403", typeof GetPluginCatalog403.Type>>
  /**
* `{source, id}` installs the catalog pin after re-checking integrity, or `{spec,
* accept_unverified: true}` installs unreviewed code the operator owns. `202` + job id; watch at
* `GET /store/jobs/{id}`.
* 
* One package operation at a time (`409` otherwise): `bun` shares a lockfile and `node_modules`.
*/
readonly "installPlugin": <Config extends OperationConfig>(options: { readonly payload: typeof InstallPluginRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof InstallPlugin202.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"InstallPlugin400", typeof InstallPlugin400.Type> | PunktfunkError<"InstallPlugin401", typeof InstallPlugin401.Type> | PunktfunkError<"InstallPlugin403", typeof InstallPlugin403.Type> | PunktfunkError<"InstallPlugin409", typeof InstallPlugin409.Type>>
  /**
* Plugins-dir packages, with provenance and live registration. No provenance record means CLI
* install (`tier: "cli"`); absence is the answer, not a gap.
*/
readonly "listInstalledPlugins": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof ListInstalledPlugins200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"ListInstalledPlugins401", typeof ListInstalledPlugins401.Type> | PunktfunkError<"ListInstalledPlugins403", typeof ListInstalledPlugins403.Type>>
  /**
* List recent package jobs
*/
readonly "listPluginJobs": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof ListPluginJobs200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"ListPluginJobs401", typeof ListPluginJobs401.Type> | PunktfunkError<"ListPluginJobs403", typeof ListPluginJobs403.Type>>
  /**
* Poll this while `state` is `running`; `log` carries the tail of the package manager's output.
*/
readonly "getPluginJob": <Config extends OperationConfig>(id: string, options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetPluginJob200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetPluginJob401", typeof GetPluginJob401.Type> | PunktfunkError<"GetPluginJob403", typeof GetPluginJob403.Type> | PunktfunkError<"GetPluginJob404", typeof GetPluginJob404.Type>>
  /**
* Bypasses the freshness window, re-fetches all sources, returns the merged catalog.
*/
readonly "refreshPluginCatalog": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof RefreshPluginCatalog200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"RefreshPluginCatalog401", typeof RefreshPluginCatalog401.Type> | PunktfunkError<"RefreshPluginCatalog403", typeof RefreshPluginCatalog403.Type>>
  /**
* Plugins run only while the runner is on. The runner discovers units at startup, so a freshly
* installed plugin may not have appeared yet.
*/
readonly "getPluginRuntime": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetPluginRuntime200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetPluginRuntime401", typeof GetPluginRuntime401.Type> | PunktfunkError<"GetPluginRuntime403", typeof GetPluginRuntime403.Type>>
  /**
* Turn the plugin runner on or off
*/
readonly "setPluginRuntime": <Config extends OperationConfig>(options: { readonly payload: typeof SetPluginRuntimeRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof SetPluginRuntime200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"SetPluginRuntime400", typeof SetPluginRuntime400.Type> | PunktfunkError<"SetPluginRuntime401", typeof SetPluginRuntime401.Type> | PunktfunkError<"SetPluginRuntime403", typeof SetPluginRuntime403.Type>>
  /**
* List catalog sources
*/
readonly "listPluginSources": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof ListPluginSources200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"ListPluginSources401", typeof ListPluginSources401.Type> | PunktfunkError<"ListPluginSources403", typeof ListPluginSources403.Type>>
  /**
* Its entries become installable and attributed to it. They never carry `verified`; that badge is
* the built-in source alone.
*/
readonly "putPluginSource": <Config extends OperationConfig>(name: string, options: { readonly payload: typeof PutPluginSourceRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<void, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"PutPluginSource400", typeof PutPluginSource400.Type> | PunktfunkError<"PutPluginSource401", typeof PutPluginSource401.Type> | PunktfunkError<"PutPluginSource403", typeof PutPluginSource403.Type>>
  /**
* Remove a catalog source
*/
readonly "deletePluginSource": <Config extends OperationConfig>(name: string, options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<void, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"DeletePluginSource401", typeof DeletePluginSource401.Type> | PunktfunkError<"DeletePluginSource403", typeof DeletePluginSource403.Type>>
  /**
* Drops the package and its provenance, then restarts the runner. Only names the runner would
* supervise: a shared dependency cannot be ripped out of the tree.
*/
readonly "uninstallPlugin": <Config extends OperationConfig>(options: { readonly payload: typeof UninstallPluginRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof UninstallPlugin202.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"UninstallPlugin400", typeof UninstallPlugin400.Type> | PunktfunkError<"UninstallPlugin401", typeof UninstallPlugin401.Type> | PunktfunkError<"UninstallPlugin403", typeof UninstallPlugin403.Type> | PunktfunkError<"UninstallPlugin409", typeof UninstallPlugin409.Type>>
  /**
* Only for install kinds that support it. No version or URL in the body — the host
* installs the verified manifest. Poll `GET /update/status` (`job`); after restart,
* the outcome is `last_result`.
*/
readonly "applyUpdate": <Config extends OperationConfig>(options: { readonly payload: typeof ApplyUpdateRequestJson.Encoded; readonly config?: Config | undefined }) => Effect.Effect<WithOptionalResponse<typeof ApplyUpdate202.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"ApplyUpdate401", typeof ApplyUpdate401.Type> | PunktfunkError<"ApplyUpdate409", typeof ApplyUpdate409.Type>>
  /**
* One forced check per 30 s. `last_error` is a failed check; `not_published`
* is an empty channel, not a failure.
*/
readonly "forceUpdateCheck": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof ForceUpdateCheck200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"ForceUpdateCheck401", typeof ForceUpdateCheck401.Type> | PunktfunkError<"ForceUpdateCheck409", typeof ForceUpdateCheck409.Type> | PunktfunkError<"ForceUpdateCheck429", typeof ForceUpdateCheck429.Type>>
  /**
* A cache older than 6 h may kick a background refresh. The response never
* blocks on the network.
*/
readonly "getUpdateStatus": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetUpdateStatus200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetUpdateStatus401", typeof GetUpdateStatus401.Type>>
  /**
* Where to reach the browser plane
*/
readonly "getWebTransport": <Config extends OperationConfig>(options: { readonly config?: Config | undefined } | undefined) => Effect.Effect<WithOptionalResponse<typeof GetWebTransport200.Type, Config>, HttpClientError.HttpClientError | SchemaError | PunktfunkError<"GetWebTransport404", typeof GetWebTransport404.Type>>
}

export interface PunktfunkError<Tag extends string, E> {
  readonly _tag: Tag
  readonly request: HttpClientRequest.HttpClientRequest
  readonly response: HttpClientResponse.HttpClientResponse
  readonly cause: E
}

class PunktfunkErrorImpl extends Data.Error<{
  _tag: string
  cause: any
  request: HttpClientRequest.HttpClientRequest
  response: HttpClientResponse.HttpClientResponse
}> {}

export const PunktfunkError = <Tag extends string, E>(
  tag: Tag,
  cause: E,
  response: HttpClientResponse.HttpClientResponse,
): PunktfunkError<Tag, E> =>
  new PunktfunkErrorImpl({
    _tag: tag,
    cause,
    response,
    request: response.request,
  }) as any
