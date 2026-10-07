// The console's destinations, in one table (design/web-console-overhaul.md §4).
//
// Ten top-level entries plus one per UI plugin did not fit a phone, and did not fit a head
// either. Five primary destinations carry the work; everything else is Manage, one level in.
// Routes do not change — bookmarks, deep links, the tray and the Omarchy menu all point at
// them, so only labels, order and grouping move.
//
// This table is the single source: the sidebar, the phone bar and the phone "More" list all
// read it. They used to read three (`NAV`, `MOBILE_PRIMARY`, `MOBILE_OVERFLOW`), which is how
// the phone bar and the sidebar drifted apart.
import {
	Activity,
	Gamepad2,
	GaugeCircle,
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
		to: "/pairing",
		icon: Smartphone,
		label: () => m.nav_devices(),
		hint: () => m.nav_devices_hint(),
		group: "primary",
	},
	{
		to: "/displays",
		icon: MonitorPlay,
		label: () => m.nav_displays(),
		hint: () => m.nav_displays_hint(),
		group: "primary",
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
	},
	{
		to: "/profiles",
		icon: UsersRound,
		label: () => m.nav_profiles(),
		hint: () => m.nav_profiles_hint(),
		group: "manage",
	},
	{
		to: "/controllers",
		icon: Gamepad2,
		label: () => m.nav_controllers(),
		hint: () => m.nav_controllers_hint(),
		group: "manage",
	},
	{
		to: "/stats",
		icon: GaugeCircle,
		label: () => m.nav_stats(),
		hint: () => m.nav_stats_hint(),
		group: "manage",
	},
	// The page is the troubleshooting home — health checks above the log stream. The ROUTE
	// stays `/logs`: bookmarks and deep links outlive a label.
	{
		to: "/logs",
		icon: Stethoscope,
		label: () => m.nav_troubleshooting(),
		hint: () => m.nav_troubleshooting_hint(),
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

/**
 * A pin id: a plugin (`plugin:rom-manager`). A plugin's page is the only destination the
 * sidebar does not already list, so it is the only thing a pin can add.
 *
 * The prefix keeps a plugin id apart from a route — its page is `/plugins/<id>/`, and a plugin
 * called "logs" must not match Troubleshooting. Route pins an older console stored no longer
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
