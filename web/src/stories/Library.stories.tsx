import type { Meta, StoryObj } from "@storybook/react-vite";
import { ApiError } from "@/api/fetcher";
import type { PluginAccessSnapshot } from "@/api/gen/model/pluginAccessSnapshot";
import type { SeatScopeValue } from "@/api/seat";
import { QueryState } from "@/components/query-state";
import { SeatScopeView } from "@/components/seat-scope";
import { LibraryHeader } from "@/sections/Library";
import { LibraryGrid } from "@/sections/Library/LibraryGrid";
import { MigrationBanner, SourcesCard } from "@/sections/Library/Sources";
import { library } from "./lib/fixtures";
import { Routed } from "./lib/routed";

const noop = () => {};
const idle = { isLoading: false, error: null, refetch: noop };
// Cards link to the entry page, so every story renders inside a router.
const meta = {
	title: "Pages/Library",
	parameters: { layout: "padded" },
	decorators: [
		(Story) => (
			<Routed>
				<Story />
			</Routed>
		),
	],
} satisfies Meta;

export default meta;
type Story = StoryObj;

const LAUNCHER = {
	id: "steam:bigpicture",
	store: "steam",
	title: "Steam Big Picture",
	art: { portrait: null, hero: null, logo: null, header: null },
	role: "launcher" as const,
	launch: { kind: "steam_ui", value: "bigpicture" },
};

/** What the container hands the view: every handler a no-op, nothing narrowed, covers. */
const gridArgs = {
	games: { data: library, ...idle },
	launchers: [],
	total: library.length,
	platforms: [],
	hasMore: false,
	loadingMore: false,
	onMore: noop,
	query: "",
	onQuery: noop,
	platform: null,
	onPlatform: noop,
	filtered: false,
	view: "grid" as const,
	onView: noop,
	onDelete: noop,
	deletingId: null,
	onToggleHidden: noop,
	hidingId: null,
};

const SEATS = [
	{ id: "a".repeat(32), name: "Ben" },
	{ id: "b".repeat(32), name: "Guest room" },
];

/** A host with two seats of its own: the chip, on the box's library. */
const scope = (seat: SeatScopeValue["seat"]): SeatScopeValue => ({
	page: "library",
	seat,
	seats: SEATS,
	ownerName: "Enrico",
	pick: noop,
});

/** **Whose library** on the header: the box's own is the default. */
export const WhoseLibrary: Story = {
	render: () => (
		<SeatScopeView {...scope(null)}>
			<LibraryHeader />
		</SeatScopeView>
	),
};

/** A seat picked, whose host is not running. */
export const SeatDown: Story = {
	render: () => (
		<SeatScopeView {...scope(SEATS[0] ?? null)}>
			<div className="flex flex-col gap-card">
				<LibraryHeader />
				<QueryState
					isLoading={false}
					error={
						new ApiError(502, {
							error: "The seat didn't answer. Start it, then try again.",
						})
					}
					refetch={noop}
				>
					{null}
				</QueryState>
			</div>
		</SeatScopeView>
	),
};

export const Populated: Story = {
	render: () => <LibraryGrid {...gridArgs} />,
};

/** The same titles as lines: many more fit a screen. */
export const AsList: Story = {
	render: () => <LibraryGrid {...gridArgs} view="rows" />,
};

/** One page of a large library: the count says how much is left, and the list goes on. */
export const FirstPageOfMany: Story = {
	render: () => (
		<LibraryGrid
			{...gridArgs}
			total={5120}
			hasMore
			platforms={[
				{ platform: "PS2", count: 1840 },
				{ platform: "SNES", count: 1211 },
				{ platform: "N64", count: 388 },
			]}
		/>
	),
};

/** A search that finds nothing is a miss, not a fresh host. */
export const NoMatches: Story = {
	render: () => (
		<LibraryGrid
			{...gridArgs}
			games={{ data: [], ...idle }}
			total={0}
			query="zzz"
			filtered
		/>
	),
};

/** Launcher entries (design D4) get their own rail above the grid. */
export const WithLaunchers: Story = {
	render: () => <LibraryGrid {...gridArgs} launchers={[LAUNCHER]} />,
};

/**
 * A hidden title, as only the operator's console ever sees it — every other surface has it filtered
 * out upstream. The poster dims but the badge and the un-hide button stay at full contrast, because
 * this card is the only route back.
 */
export const WithHidden: Story = {
	render: () => (
		<LibraryGrid
			{...gridArgs}
			games={{
				data: library.map((g, i) => (i === 1 ? { ...g, hidden: true } : g)),
				...idle,
			}}
		/>
	),
};

export const Empty: Story = {
	render: () => (
		<LibraryGrid {...gridArgs} games={{ data: [], ...idle }} total={0} />
	),
};

/** A catalog row for the "Add a source" rail — only the fields the card actually reads. */
const catalogEntry = (
	over: Partial<Parameters<typeof SourcesCard>[0]["available"][number]>,
) =>
	({
		id: "steam",
		pkg: "@punktfunk/plugin-steam",
		title: "Steam",
		description: "Steam library scanner",
		author: "unom",
		version: "0.1.0",
		source: "unom",
		tier: "verified",
		platforms: [],
		compatible: true,
		update_available: false,
		categories: ["library"],
		...over,
	}) as Parameters<typeof SourcesCard>[0]["available"][number];

const sourcesArgs = {
	available: [],
	running: new Set<string>(),
	busyId: null,
	installBusy: false,
	activeFilter: null,
	onToggle: noop,
	onFilter: noop,
	onSettings: noop,
	onPurge: noop,
	onInstall: noop,
};

const STEAM_SOURCE = {
	id: "steam",
	label: "Steam",
	enabled: true,
	origin: "plugin" as const,
	provider: "steam",
	entries: 7,
};

const steamAccess = (write = false): PluginAccessSnapshot[] => [
	{
		plugin: "steam",
		grants: [],
		pending: [
			{
				path: "/mnt/games1",
				write,
				reason: "Steam library folder",
				at: "2026-09-18T12:00:00Z",
			},
			{
				path: "/mnt/games2",
				write: false,
				reason: "Steam library folder",
				at: "2026-09-18T12:00:01Z",
			},
		],
		denied: [],
	},
];

/** The bridge-release shape: built-in scanners only, one turned off. */
export const Sources: Story = {
	render: () => (
		<SourcesCard
			{...sourcesArgs}
			sources={[
				{ id: "steam", label: "Steam", enabled: true, origin: "builtin" },
				{ id: "lutris", label: "Lutris", enabled: false, origin: "builtin" },
				{
					id: "heroic",
					label: "Heroic (Epic / GOG / Amazon)",
					enabled: true,
					origin: "builtin",
				},
			]}
		/>
	),
};

/** Mid-migration: a claimed plugin source beside the remaining built-ins, one plugin stopped. */
export const SourcesWithPlugins: Story = {
	render: () => (
		<SourcesCard
			{...sourcesArgs}
			sources={[
				{
					id: "steam",
					label: "Steam",
					enabled: true,
					origin: "plugin",
					provider: "steam",
					entries: 214,
				},
				{
					id: "lutris",
					label: "Lutris",
					enabled: false,
					origin: "plugin",
					provider: "lutris",
					entries: 12,
				},
				{
					id: "heroic",
					label: "Heroic (Epic / GOG / Amazon)",
					enabled: true,
					origin: "builtin",
				},
			]}
			running={new Set(["steam"])}
			available={[
				catalogEntry({ pkg: "@punktfunk/plugin-heroic", title: "Heroic" }),
			]}
		/>
	),
};

export const SourcePendingReadAccess: Story = {
	render: () => (
		<SourcesCard
			{...sourcesArgs}
			sources={[STEAM_SOURCE]}
			running={new Set(["steam"])}
			access={steamAccess()}
			accessInitiallyOpen
		/>
	),
};

export const SourcePendingWriteAccess: Story = {
	render: () => (
		<SourcesCard
			{...sourcesArgs}
			sources={[STEAM_SOURCE]}
			running={new Set(["steam"])}
			access={steamAccess(true)}
			accessInitiallyOpen
		/>
	),
};

/** A fresh host after extraction: nothing installed, two launchers detected on this box. */
export const SourcesEmptyWithDetected: Story = {
	render: () => (
		<SourcesCard
			{...sourcesArgs}
			sources={[]}
			available={[
				catalogEntry({ detected: true }),
				catalogEntry({
					pkg: "@punktfunk/plugin-lutris",
					title: "Lutris",
					detected: true,
				}),
				catalogEntry({
					pkg: "@punktfunk/plugin-heroic",
					title: "Heroic",
					detected: false,
				}),
			]}
		/>
	),
};

/** The bridge-release nudge — one button per still-built-in scanner, never an auto-install. */
export const Migration: Story = {
	render: () => (
		<MigrationBanner
			rows={[
				{
					source: {
						id: "steam",
						label: "Steam",
						enabled: true,
						origin: "builtin",
					},
					entry: catalogEntry({}),
				},
				{
					source: {
						id: "lutris",
						label: "Lutris",
						enabled: true,
						origin: "builtin",
					},
					entry: catalogEntry({
						pkg: "@punktfunk/plugin-lutris",
						id: "lutris",
						title: "Lutris",
					}),
				},
			]}
			busy={false}
			onInstall={noop}
		/>
	),
};
