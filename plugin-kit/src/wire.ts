// The library-provider wire schemas — a browser-safe module (no node imports) so plugin
// CONTRACTS can share these types with their UIs. Mirrors the host's `ProviderEntryInput`
// (crates/punktfunk-host mgmt/library.rs). Identity codecs: plain JSON shapes, so values
// pass through unencoded; the value is the shared type + authoring validation.
import { Schema } from "effect";

export const Artwork = Schema.Struct({
	portrait: Schema.optionalKey(Schema.NullOr(Schema.String)),
	hero: Schema.optionalKey(Schema.NullOr(Schema.String)),
	logo: Schema.optionalKey(Schema.NullOr(Schema.String)),
	header: Schema.optionalKey(Schema.NullOr(Schema.String)),
});
export type Artwork = typeof Artwork.Type;

/**
 * How the host should launch a title. **The host owns this vocabulary** — it validates the value
 * per kind and builds the actual URI / command line itself, so a plugin only ever supplies a
 * validated value, never a command. That is the security invariant behind the whole provider lane:
 * a client sends an entry id, and the host resolves what to run.
 *
 * `kind` is a plain string rather than a union so the kit never has to ship a release to keep up
 * with a host that grew a new kind. The kinds the host understands today:
 *
 * | kind | value | platforms |
 * |---|---|---|
 * | `command` | a shell command (operator-trust tier) | both |
 * | `steam_appid` | digits — an appid, or a 64-bit non-Steam-shortcut game id | both |
 * | `steam_ui` | `bigpicture` \| `desktop` — opens the Steam client itself | both |
 * | `launcher_ui` | which launcher UI to open: `heroic` \| `heroic-console` \| `lutris` on linux; `playnite` \| `epic` \| `gog` \| `xbox` on windows | both |
 * | `lutris_id` | digits — a pga.db game id | linux |
 * | `heroic` | `<runner>:<appName>`, runner ∈ legendary/gog/nile | linux |
 * | `epic` | `<namespace>:<catalogItemId>:<appName>` or a bare appName | windows |
 * | `gog` | `exe \t args \t workdir` | windows |
 * | `aumid` | `<PFN>!<AppId>` | windows |
 * | `xbox` | `<Identity>!<AppId>` from `MicrosoftGame.config`; the host completes the AUMID | windows |
 * | `playnite` | a Playnite game GUID | windows |
 * | `uplay` | digits — a Ubisoft Connect game id | windows |
 * | `amazon` | an Amazon Games product id (`amzn1.adg.product.…`) | windows |
 * | `battlenet` | a Battle.net launch code (`WTCG`, `Pro`, `Fen`, …), case kept | windows |
 * | `gamebar` | an exe's absolute path; the host runs it only if a signed-in user's Game Bar list names it | windows |
 * | `desktop_id` | an installed `.desktop` entry's id; the host reads its `Exec` | linux |
 * | `exec` | the name of an `exec` template in THIS plugin's manifest — see below | both |
 *
 * `exec` is how a tile the host cannot name on its own (a ROM through whichever emulator the
 * operator configured) still launches. The template — program and argv — lives in the `punktfunk`
 * block of your package.json, which ships in the reviewed tarball; the entry only supplies values
 * for its `{param}` placeholders in `args`, and the host checks each against the character class
 * the template declares. A plugin never composes a command line, and nothing it says at runtime
 * widens what may run.
 *
 * An unknown kind is accepted on the wire and simply yields no launch recipe on that host, so a
 * plugin targeting a newer host degrades to an unlaunchable tile rather than a failed reconcile.
 */
export const LaunchSpec = Schema.Struct({
	kind: Schema.String,
	value: Schema.String,
	/** Values for an `exec` template's `{param}` placeholders. */
	args: Schema.optionalKey(
		Schema.Array(Schema.Struct({ name: Schema.String, value: Schema.String })),
	),
});
export type LaunchSpec = typeof LaunchSpec.Type;

/**
 * Whether an entry is an ordinary title or the launcher application itself (Steam Big Picture,
 * Heroic, Playnite fullscreen). Launcher entries launch, lease and list exactly like games; a
 * console or client that knows the field groups them into their own rail, and one that doesn't
 * renders them as plain tiles.
 */
export const GameRole = Schema.Literals(["game", "launcher"]);
export type GameRole = typeof GameRole.Type;

/**
 * The brand marks the shipped clients draw for a launcher tile. A plugin puts one of these in an
 * entry's `icon` and every client resolves it against the art it bundles
 * (`assets/launcher-icons` — provenance and licensing in that directory's README).
 *
 * Not a union type on purpose, exactly like {@link LaunchSpec}'s `kind`: a client that has never
 * heard of a token falls back to naming the launcher on an accent face — which is what every
 * launcher tile looked like before icons existed — so a plugin naming a mark a *newer* client
 * ships must not fail to typecheck against an older kit.
 */
export const LAUNCHER_ICONS = [
	"steam",
	"lutris",
	"heroic",
	"playnite",
	"epic",
	"gog",
	"xbox",
] as const;

export const PrepStep = Schema.Struct({
	do: Schema.String,
	undo: Schema.optionalKey(Schema.NullOr(Schema.String)),
});
export type PrepStep = typeof PrepStep.Type;

/**
 * How the host should recognize a title's process once it is running.
 *
 * Every field is optional, and omitting the whole thing is fine: the host tracks the process it
 * spawns for the entry anyway. It matters when your launch command hands off and exits — a launcher
 * client, a `flatpak run`, a front-end that starts an emulator — because then the host has nothing
 * left to watch, and the two behaviors this feeds ("end the session when the game exits" and "end the
 * game when the session ends") go quiet for that title.
 *
 * Send whatever you actually know. `install_dir` is the one worth sending if you send only one: any
 * process running from under it counts as the game.
 */
export const DetectHint = Schema.Struct({
	/** Where the title is installed (absolute path on the host). */
	install_dir: Schema.optionalKey(Schema.NullOr(Schema.String)),
	/** The game's own executable (absolute path on the host). */
	exe: Schema.optionalKey(Schema.NullOr(Schema.String)),
	/** The executable's file name (`Hades.exe`), when its location isn't fixed. Weakest signal. */
	process_name: Schema.optionalKey(Schema.NullOr(Schema.String)),
	/**
	 * The Steam appid, for a title Steam itself installed. On Linux this is the **sharpest** signal
	 * there is: Steam wraps every launch — native or Proton — in `reaper SteamLaunch AppId=<appid>`,
	 * whose lifetime is exactly the game's. Send it if you have it.
	 */
	steam_appid: Schema.optionalKey(Schema.NullOr(Schema.Number)),
	/**
	 * An environment variable the launcher stamps on the game's process. Load-bearing for launchers
	 * that run games under Proton/Wine, where the process tree tells you very little (Heroic's
	 * `HEROIC_APP_NAME` is the verified case). Omit `value` to match on the key's mere presence —
	 * only safe for a launcher that runs one game at a time.
	 */
	env_marker: Schema.optionalKey(
		Schema.NullOr(
			Schema.Struct({
				/** `[A-Za-z0-9_]{1,64}` — the host rejects anything else. */
				key: Schema.String,
				/** At most 256 chars. */
				value: Schema.optionalKey(Schema.NullOr(Schema.String)),
			}),
		),
	),
});
export type DetectHint = typeof DetectHint.Type;

/** Descriptive metadata, flat on the wire beside `title` (mirrors the host's flattened
 * `GameMeta`). All fields optional; values are free-form display strings — the host does not
 * normalize platform/genre vocabularies. */
export const GameMeta = Schema.Struct({
	/** The system the title runs on — `"PS2"`, `"Xbox 360"`, `"SNES"`, … */
	platform: Schema.optionalKey(Schema.NullOr(Schema.String)),
	/** Short blurb for a details pane. */
	description: Schema.optionalKey(Schema.NullOr(Schema.String)),
	developer: Schema.optionalKey(Schema.NullOr(Schema.String)),
	publisher: Schema.optionalKey(Schema.NullOr(Schema.String)),
	/** Year of first release. */
	release_year: Schema.optionalKey(Schema.NullOr(Schema.Number)),
	/** Genre taxonomy from the metadata source (`"RPG"`, `"Platformer"`, …). */
	genres: Schema.optionalKey(Schema.Array(Schema.String)),
	/** Free-form organizational labels (`"co-op"`, `"kids"`, …). */
	tags: Schema.optionalKey(Schema.Array(Schema.String)),
	/** Release region — `"NTSC-U"`, `"PAL"`, `"NTSC-J"`. */
	region: Schema.optionalKey(Schema.NullOr(Schema.String)),
	/** Maximum simultaneous (local) players. */
	players: Schema.optionalKey(Schema.NullOr(Schema.Number)),
});
export type GameMeta = typeof GameMeta.Type;

/**
 * Catalog ids an Art & Metadata source matches on, set by the plugin that lists the entry:
 * `steam` → appid, `gog` → product id, `epic` → catalog item id, `libretro` →
 * `<libretro system>/<No-Intro name>`, `sgdb` → SteamGridDB game id. Keys `[a-z0-9_]{1,16}`,
 * values at most 256 characters, at most eight; the host drops a bad pair.
 */
export const EntryIds = Schema.Record(Schema.String, Schema.String);
export type EntryIds = typeof EntryIds.Type;

export const ProviderEntry = Schema.Struct({
	external_id: Schema.String,
	title: Schema.String,
	art: Schema.optionalKey(Artwork),
	launch: Schema.optionalKey(Schema.NullOr(LaunchSpec)),
	prep: Schema.optionalKey(Schema.Array(PrepStep)),
	detect: Schema.optionalKey(DetectHint),
	/** `"game"` (default) or `"launcher"` — see {@link GameRole}. */
	role: Schema.optionalKey(GameRole),
	/**
	 * Which brand mark a client should draw for this entry — a **token** ({@link LAUNCHER_ICONS}),
	 * never image bytes and never a URL. `[a-z][a-z0-9-]{0,31}`; the host rejects anything else.
	 *
	 * This is what makes a launcher tile look like its launcher. Launcher entries ship no cover art
	 * by design — a launcher's own icon is square, clients cover-crop a 2:3 poster, and the crop
	 * turns a mark into a strip — so before this they were the launcher's name on a flat accent
	 * face. Naming the mark instead of sending it keeps the glyph vector at any tile size, lets it
	 * take the tile's ink, and adds nothing to a reconcile payload that is already body-limited.
	 *
	 * Sending art instead is not an option the host leaves open: its art proxy serves raster
	 * containers only and refuses SVG outright, because SVG is script-capable XML and the web
	 * console renders library art in a browser.
	 *
	 * A token no client bundles is not an error — that tile just falls back to its name. To get a
	 * new launcher's mark shipped, open a PR adding the master to `assets/launcher-icons`.
	 *
	 * Set it on your `launchers(cfg)` entries. Ordinary titles may carry one, but shouldn't: a game
	 * has real cover art, which beats a brand mark every time.
	 */
	icon: Schema.optionalKey(Schema.String),
	ids: Schema.optionalKey(EntryIds),
	...GameMeta.fields,
});
export type ProviderEntry = typeof ProviderEntry.Type;

/** One of an entry's four art slots. */
export const ArtKind = Schema.Literals(["portrait", "hero", "logo", "header"]);
export type ArtKind = typeof ArtKind.Type;
export const ART_KINDS: ReadonlyArray<ArtKind> = [
	"portrait",
	"hero",
	"logo",
	"header",
];

/** A `GameMeta` field a metadata source may fill. */
export type MetaField = keyof GameMeta;

/**
 * One row of an Art & Metadata source's push (`PUT /library/metadata/{source}`). Art is
 * `http(s)` URLs only; the host fetches and keeps them like a provider's CDN art.
 */
export const MetadataEntry = Schema.Struct({
	/** Library id, as `GET /library` lists it. */
	id: Schema.String,
	art: Schema.optionalKey(Artwork),
	meta: Schema.optionalKey(GameMeta),
});
export type MetadataEntry = typeof MetadataEntry.Type;
