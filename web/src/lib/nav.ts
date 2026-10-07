// The console's destinations, in one table (design/web-console-structure-2026-10.md §4).
//
// Six primary destinations carry the work; everything else is Manage, one level in. The URL is
// the structure: a page's path is its place here, a segment is a child path, and a renamed page
// moves (its old path redirects for one release, `routes/*.tsx`).
//
// This table is the single source: the sidebar, the phone bar and the phone "More" list all
// read it.
import {
	Activity,
	LibraryBig,
	type LucideIcon,
	MonitorPlay,
	Puzzle,
	Server,
	Settings,
	Smartphone,
	Stethoscope,
	UsersRound,
	Workflow,
} from "lucide-react";
import { isStringArray, useLocalPref } from "@/lib/prefs";
import { m } from "@/paraglide/messages";

export type NavGroup = "primary" | "manage";

export interface NavEntry {
	to: string;
	icon: LucideIcon;
	label: () => string;
	/** One line for the phone's More list — what the page is for, not what it contains. */
	hint: () => string;
	group: NavGroup;
	/** A primary page the phone bar has no room for: it opens from More, above Manage. */
	sheet?: boolean;
	/**
	 * Active only on an exact path match. `/` would otherwise match everything, and `/plugins`
	 * is the store's index route sitting under `/plugins/<id>`, a plugin's own UI.
	 */
	exact?: boolean;
}

export const NAV: readonly NavEntry[] = [
	{
		to: "/",
		icon: Activity,
		label: () => m.nav_home(),
		hint: () => m.nav_home_hint(),
		group: "primary",
		exact: true,
	},
	{
		to: "/profiles",
		icon: UsersRound,
		label: () => m.nav_profiles(),
		hint: () => m.nav_profiles_hint(),
		group: "primary",
	},
	{
		to: "/devices",
		icon: Smartphone,
		label: () => m.nav_devices(),
		hint: () => m.nav_devices_hint(),
		group: "primary",
	},
	// The map wants width, so on a phone Displays and Host open from More.
	{
		to: "/displays",
		icon: MonitorPlay,
		label: () => m.nav_displays(),
		hint: () => m.nav_displays_hint(),
		group: "primary",
		sheet: true,
	},
	{
		to: "/library",
		icon: LibraryBig,
		label: () => m.nav_library(),
		hint: () => m.nav_library_hint(),
		group: "primary",
	},
	{
		to: "/host",
		icon: Server,
		label: () => m.nav_host(),
		hint: () => m.nav_host_hint(),
		group: "primary",
		sheet: true,
	},
	// Lands on the first segment; every `/diagnostics/*` path lights it.
	{
		to: "/diagnostics",
		icon: Stethoscope,
		label: () => m.nav_diagnostics(),
		hint: () => m.nav_diagnostics_hint(),
		group: "manage",
	},
	{
		to: "/automation",
		icon: Workflow,
		label: () => m.nav_automation(),
		hint: () => m.nav_automation_hint(),
		group: "manage",
	},
	{
		to: "/plugins",
		icon: Puzzle,
		label: () => m.nav_plugins(),
		hint: () => m.nav_plugins_hint(),
		group: "manage",
		exact: true,
	},
	{
		to: "/settings",
		icon: Settings,
		label: () => m.nav_settings(),
		hint: () => m.nav_settings_hint(),
		group: "manage",
	},
];

export const PRIMARY = NAV.filter((n) => n.group === "primary");
export const MANAGE = NAV.filter((n) => n.group === "manage");
/** The phone bar's four; the rest of the primary pages open from More. */
export const PHONE_BAR = PRIMARY.filter((n) => !n.sheet);
export const PHONE_SHEET = PRIMARY.filter((n) => n.sheet);

/**
 * A pin id: a plugin (`plugin:rom-manager`). A plugin's page is the only destination the
 * sidebar does not already list, so it is the only thing a pin can add.
 *
 * The prefix keeps a plugin id apart from a route — its page is `/plugins/<id>/`, and a plugin
 * called "diagnostics" must not match Diagnostics. Route pins an older console stored no longer
 * resolve.
 */
export const PLUGIN_PIN = "plugin:";
export const pluginPin = (id: string) => `${PLUGIN_PIN}${id}`;
export const pinnedPluginId = (pin: string) =>
	pin.startsWith(PLUGIN_PIN) ? pin.slice(PLUGIN_PIN.length) : undefined;

/**
 * Which plugin pages the operator put in the sidebar.
 *
 * Per browser, by design (D8): a phone and a desk want different shortcuts, and nothing here
 * is worth a round trip to the host. An id that no longer resolves — a plugin since removed —
 * is dropped where it is rendered rather than pruned on read, so uninstalling and reinstalling
 * a plugin does not silently lose its pin.
 */
const NO_PINS: string[] = [];
export const usePins = () => useLocalPref("pf-nav", NO_PINS, isStringArray);

/** Add or remove `id`, preserving order. */
export const togglePin = (pins: string[], id: string) =>
	pins.includes(id) ? pins.filter((p) => p !== id) : [...pins, id];

/** The slice of a plugin a pin needs — kept structural so a test needs no API fixture. */
export interface PinnablePlugin {
	id: string;
	title: string;
	ui?: { icon?: string | null } | null;
}

/**
 * The pinned plugins, in the operator's order.
 *
 * A pin nothing resolves — a plugin since uninstalled, or a route an older console let you pin —
 * is dropped here rather than pruned on read: uninstalling and reinstalling a plugin would
 * otherwise silently lose its pin.
 */
export function resolvePins(
	pins: readonly string[],
	plugins: readonly PinnablePlugin[],
): PinnablePlugin[] {
	return pins.flatMap((id) => {
		const plugin = plugins.find((p) => p.id === pinnedPluginId(id));
		return plugin ? [plugin] : [];
	});
}
