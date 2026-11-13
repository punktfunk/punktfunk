// The plugin directory the console reads to grow its nav (plugin-ui-surface §5), over the
// generated `listPlugins` client: icons, which plugins earn a nav entry, and fast polling after
// an install.
import {
	Blocks,
	Boxes,
	Clapperboard,
	Database,
	FolderCog,
	Gamepad2,
	Home,
	type LucideIcon,
	Plug,
	Puzzle,
	Wrench,
} from "lucide-react";
import type { PluginSummary } from "@/api/gen/model";
import { useListPlugins } from "@/api/gen/plugins/plugins";
import { usePerson } from "@/lib/person";
import { usePersonPlugins } from "./person";

/** A game source: listed under Library → Game sources. */
export const LIBRARY_CATEGORY = "library";
/** An Art & Metadata source: listed under Library → Art & Metadata. */
export const METADATA_CATEGORY = "metadata";

// A curated lucide set for plugin nav icons. Importing lucide's full dynamic icon map would defeat
// tree-shaking (U-S4), so a plugin picks a name from here; anything unknown falls back to Puzzle.
const ICONS: Record<string, LucideIcon> = {
	"gamepad-2": Gamepad2,
	puzzle: Puzzle,
	wrench: Wrench,
	database: Database,
	home: Home,
	blocks: Blocks,
	boxes: Boxes,
	plug: Plug,
	"folder-cog": FolderCog,
	clapperboard: Clapperboard,
};

/**
 * Resolve a registered icon name to a component (Puzzle fallback).
 *
 * `name` comes from a plugin's own registration, so it is untrusted input to a lookup on a plain
 * object — and a plain object inherits from Object.prototype. `ICONS["constructor"]` is `Object`,
 * which is truthy, so a `?? Puzzle` fallback never fires and React is handed `Object` as a
 * component: it throws out of render, and because this runs inside the AppShell nav that takes
 * down every page of the console. `Object.hasOwn` keeps the lookup to keys we actually declared.
 */
export const pluginIcon = (name?: string | null): LucideIcon => {
	if (!name || !Object.hasOwn(ICONS, name)) return Puzzle;
	return ICONS[name] ?? Puzzle;
};

const IDLE_POLL_MS = 30_000;
const BOOST_POLL_MS = 2_000;
/** How long to keep polling fast after something changed the installed set. */
const BOOST_MS = 60_000;

/**
 * Until this timestamp, poll the directory fast.
 *
 * A finished install is NOT the moment the plugin appears: the host restarts the scripting runner
 * afterwards, and the plugin only registers its UI once that comes back — several seconds later,
 * and after any one-shot invalidation has already run and found the old list. So the nav sat
 * unchanged until the 30 s idle poll happened to land, which in practice meant "until I reloaded".
 *
 * Module-level rather than component state because the two things that need to trigger it (a store
 * job settling, a `plugins.changed`/`store.changed` event) both live outside the nav.
 */
let boostUntil = 0;

/** Poll the plugin directory fast for a while — call after anything that changes what's installed. */
export function boostPluginPolling(): void {
	boostUntil = Date.now() + BOOST_MS;
}

/**
 * Live plugin registrations, polled (and refetched on window focus) so the nav stays current. In
 * a person's console, their seat host's, when it runs plugins of theirs.
 */
export const usePlugins = () => {
	const person = usePerson();
	const theirs = usePersonPlugins(person).offered;
	return useListPlugins({
		query: {
			enabled: !person || theirs,
			refetchInterval: () =>
				Date.now() < boostUntil ? BOOST_POLL_MS : IDLE_POLL_MS,
			refetchOnWindowFocus: true,
		},
	});
};

/**
 * The plugins that get a **nav entry**: those serving a page, minus the library and metadata ones.
 *
 * A plugin with only a settings form or an entry tab still serves a UI port, and its
 * `/plugins/$pluginId/$` route still resolves, so a deep link keeps working — it simply isn't
 * advertised in the sidebar.
 */
export const uiPlugins = (list: PluginSummary[] | undefined): PluginSummary[] =>
	(list ?? []).filter(
		(p) =>
			p.ui &&
			p.ui.page !== false &&
			p.category !== LIBRARY_CATEGORY &&
			p.category !== METADATA_CATEGORY,
	);

/** The plugins that add a tab to every library entry's page. */
export const gamePlugins = (
	list: PluginSummary[] | undefined,
): PluginSummary[] => (list ?? []).filter((p) => p.ui?.game === true);
