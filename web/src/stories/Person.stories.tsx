import type { Meta, StoryObj } from "@storybook/react-vite";
import {
	createMemoryHistory,
	createRootRoute,
	createRoute,
	createRouter,
	RouterProvider,
} from "@tanstack/react-router";
import { Toaster, toast } from "@unom/ui/toast";
import {
	type ComponentProps,
	type FC,
	type ReactNode,
	useEffect,
	useMemo,
	useState,
} from "react";
import { userEvent, within } from "storybook/test";
import { ApiError } from "@/api/fetcher";
import { getGetEmulatorsQueryKey } from "@/api/gen/emulators/emulators";
import { getGetHooksQueryKey } from "@/api/gen/hooks/hooks";
import { getGetLibraryOverridesQueryKey } from "@/api/gen/library/library";
import type {
	CatalogEntry,
	CatalogResponse,
	EmulatorStatus,
	InstalledView,
	RuntimeView,
	ScannerInfo,
} from "@/api/gen/model";
import type { OperatorGameEntry } from "@/api/gen/model/operatorGameEntry";
import { getListPluginsQueryKey } from "@/api/gen/plugins/plugins";
import {
	getGetPluginCatalogQueryKey,
	getGetPluginRuntimeQueryKey,
	getListInstalledPluginsQueryKey,
	getListPluginJobsQueryKey,
} from "@/api/gen/store/store";
import { personScope, type SeatPage } from "@/api/seat";
import { PersonFrame } from "@/components/app-shell";
import { QueryState } from "@/components/query-state";
import { SeatScopeView } from "@/components/seat-scope";
import { Card, CardContent } from "@/components/ui/card";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { apiErrorMessage } from "@/lib/errors";
import { type Closed, PersonContext, type PersonMe } from "@/lib/person";
import { m } from "@/paraglide/messages";
import { AutomationHere } from "@/sections/Automation";
import { LibraryHeader } from "@/sections/Library";
import { EmulatorsCard } from "@/sections/Library/Emulators";
import {
	emptyForm,
	type FormState,
	formFrom,
} from "@/sections/Library/Entry/model";
import { EntryView } from "@/sections/Library/Entry/view";
import {
	LibraryGrid,
	type LibraryGridProps,
} from "@/sections/Library/LibraryGrid";
import { SourceSettingsDialog } from "@/sections/Library/SourceSettings";
import { SourcesCard } from "@/sections/Library/Sources";
import { PersonClosed } from "@/sections/Person/Closed";
import { PersonHomeView } from "@/sections/Person/Home";
import type { GameExitView } from "@/sections/Profiles/session";
import { SectionSettings } from "@/sections/Settings";
import { StoreHere } from "@/sections/Store";
import { library, overridesDoc } from "./lib/fixtures";
import { Seeded } from "./lib/seeded";

const noop = () => {};

/**
 * A person's own console, opened from their desktop: Anna plays on a desktop the box made for
 * her on the computer "den-pc", which Enrico runs. Every page sits in her frame.
 */
const meta = {
	title: "Pages/Person",
	parameters: { layout: "fullscreen" },
} satisfies Meta;

export default meta;
type Story = StoryObj;

/** A stand-in photo: a figure on her accent. */
const photo = `data:image/svg+xml;utf8,${encodeURIComponent(
	`<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" fill="#c2410c"/><circle cx="32" cy="25" r="11" fill="#fed7aa"/><path d="M12 64c2-14 10-21 20-21s18 7 20 21z" fill="#fed7aa"/></svg>`,
)}`;

const ANNA: PersonMe = {
	profile: "0b7d5e2a91c4",
	name: "Anna",
	accent: "#c2410c",
	owner: "Enrico",
	own: false,
	avatar: photo,
	host: "den-pc",
	expires_at: 1_900_000_000,
};

/** Oma, on her own account: a game's exit always returns to her desktop. */
const OMA: PersonMe = {
	...ANNA,
	profile: "3d5f7b9a1c3e",
	name: "Oma",
	accent: "#0f766e",
	own: true,
	avatar: null,
};

/** A router at `path`, so the frame's links resolve and mark the page in view. */
const At: FC<{ path: string; children: ReactNode }> = ({ path, children }) => {
	const router = useMemo(() => {
		const root = createRootRoute({ component: () => <>{children}</> });
		const any = createRoute({
			getParentRoute: () => root,
			path: "$",
			component: () => null,
		});
		return createRouter({
			routeTree: root.addChildren([any]),
			history: createMemoryHistory({ initialEntries: [path] }),
		});
	}, [children, path]);
	// biome-ignore lint/suspicious/noExplicitAny: a throwaway router, not the app's typed tree
	return <RouterProvider router={router as any} />;
};

/**
 * Page `page` of `me`'s console at `path`, in the frame. Anna's desktop runs plugins of hers, so
 * her frame lists Plugins; Oma's own account runs none.
 */
const Framed: FC<{
	path: string;
	page?: SeatPage;
	me?: PersonMe;
	children: ReactNode;
}> = ({ path, page, me = ANNA, children }) => (
	<At path={path}>
		<PersonFrame
			me={me}
			pathname={path}
			plugins={!me.own}
			leaving={false}
			onSignOut={noop}
		>
			<PersonContext.Provider value={me}>
				{page ? (
					<SeatScopeView {...personScope(page, me)}>{children}</SeatScopeView>
				) : (
					children
				)}
			</PersonContext.Provider>
		</PersonFrame>
	</At>
);

// --- Home and the game-exit choice ------------------------------------------------------------

const exit = (
	configured: boolean,
	end: boolean,
): ComponentProps<typeof GameExitView> => ({
	state: {
		configured,
		enforced: ["session_on_game_exit"],
		settings: { session_on_game_exit: end },
	},
	loading: false,
	error: null,
	busy: false,
	onPick: noop,
});

/** First thing she sees from her desktop: whose console, on which computer, and her one choice. */
export const Home: Story = {
	render: () => (
		<Framed path="/">
			<PersonHomeView me={ANNA} exit={exit(false, true)} />
		</Framed>
	),
};

/** Her game-exit choice, set to her own: Keep streaming. */
export const GameExitChoice: Story = {
	render: () => (
		<Framed path="/">
			<PersonHomeView me={ANNA} exit={exit(true, false)} />
		</Framed>
	),
};

/** On her own account the exit always returns to her desktop: a sentence, no control. */
export const HomeOwnAccount: Story = {
	render: () => (
		<Framed path="/" me={OMA}>
			<PersonHomeView me={OMA} exit={null} />
		</Framed>
	),
};

/** The frame while it learns whose console it is. */
export const Opening: Story = {
	render: () => (
		<At path="/">
			<PersonFrame me={null} pathname="/" leaving={false} onSignOut={noop}>
				<QueryState isLoading error={null}>
					{null}
				</QueryState>
			</PersonFrame>
		</At>
	),
};

// --- Her library -------------------------------------------------------------------------------

const cover = (hue: number) =>
	`data:image/svg+xml;utf8,${encodeURIComponent(
		`<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 40 60"><rect width="40" height="60" fill="hsl(${hue} 40% 34%)"/><circle cx="20" cy="24" r="9" fill="hsl(${hue} 55% 68%)"/></svg>`,
	)}`;

/** The shared titles as she has them, and two games only she has. */
const hers: OperatorGameEntry[] = [
	...library.map((g, i): OperatorGameEntry => {
		const art = { ...g.art, portrait: cover(i * 67) };
		if (g.id === "steam:1086940")
			return {
				...g,
				art,
				overrides: { lines: 1440, hz: 120, hdr: true },
				profile: { overrides: true },
			};
		if (g.id === "steam:413150") return { ...g, art, hidden: true };
		return { ...g, art };
	}),
	{
		id: "own:3f9a0c1b2d4e",
		store: "custom",
		title: "Prism Launcher",
		art: { portrait: cover(110) },
		platform: "PC",
		own: true,
	},
	{
		id: "own:7c2e9b4a1f60",
		store: "custom",
		title: "Celeste (itch.io)",
		art: {},
		own: true,
	},
].sort((a, b) => a.title.localeCompare(b.title));

const grid: LibraryGridProps = {
	games: { data: hers, isLoading: false, error: null, refetch: noop },
	launchers: [],
	total: hers.length,
	platforms: [],
	hasMore: false,
	loadingMore: false,
	onMore: noop,
	query: "",
	onQuery: noop,
	platform: null,
	onPlatform: noop,
	filtered: false,
	view: "grid",
	onView: noop,
	onDelete: noop,
	deletingId: null,
	onToggleHidden: noop,
	hidingId: null,
	who: ANNA.name,
};

const LibraryPage: FC<{ children: ReactNode }> = ({ children }) => (
	<Framed path="/library" page="library">
		<div className="flex flex-col gap-card">
			<LibraryHeader />
			<Tabs value="games">
				<TabsList>
					<TabsTrigger value="games">{m.library_tab_games()}</TabsTrigger>
					<TabsTrigger value="sources">{m.library_tab_sources()}</TabsTrigger>
				</TabsList>
				<TabsContent value="games" className="flex flex-col gap-card">
					{children}
				</TabsContent>
			</Tabs>
		</div>
	</Framed>
);

/** Her library: who adds the shared games, her own two marked, Add for her own. No pick. */
export const Library: Story = {
	render: () => (
		<LibraryPage>
			<LibraryGrid {...grid} />
		</LibraryPage>
	),
};

/** Her library while it loads. */
export const LibraryLoading: Story = {
	render: () => (
		<LibraryPage>
			<LibraryGrid
				{...grid}
				games={{ data: undefined, isLoading: true, error: null }}
			/>
		</LibraryPage>
	),
};

/** Nothing listed for her yet. */
export const LibraryEmpty: Story = {
	render: () => (
		<LibraryPage>
			<LibraryGrid
				{...grid}
				games={{ data: [], isLoading: false, error: null, refetch: noop }}
				total={0}
				onSources={noop}
			/>
		</LibraryPage>
	),
};

// --- Her own games ---------------------------------------------------------------------------

/** A game only she has, typed and ready: no password, the command runs as her. */
const typed: FormState = {
	...emptyForm,
	title: "Prism Launcher",
	command: "flatpak run org.prismlauncher.PrismLauncher",
	platform: "PC",
};

const Entry: FC<{ draft: FormState; error?: string; tab?: string }> = ({
	draft: start,
	error,
	tab: first = "information",
}) => {
	const [draft, setDraft] = useState(start);
	const [tab, setTab] = useState(first);
	return (
		<Framed path="/library/new" page="library">
			<Seeded
				data={[[getGetLibraryOverridesQueryKey({ as: ANNA.profile }), {}]]}
			>
				<EntryView
					entry={null}
					draft={draft}
					baseline={emptyForm}
					readOnly={false}
					set={(key, value) => setDraft((d) => ({ ...d, [key]: value }))}
					tab={tab}
					onTab={setTab}
					header={{
						storeName: null,
						managedBy: null,
						who: ANNA.name,
						own: true,
						dirty: true,
						saving: false,
						gated: false,
						password: "",
						onPassword: noop,
						onSave: noop,
						deleting: false,
						hiding: false,
						error: error ?? null,
					}}
				/>
			</Seeded>
		</Framed>
	);
};

/** Adding a game of her own: its command, and no password to type. */
export const OwnGameAdd: Story = {
	render: () => <Entry draft={typed} tab="launch" />,
};

/** Her save refused: the host's own sentence, the form as she left it. */
export const OwnGameRefused: Story = {
	render: () => (
		<Entry
			draft={typed}
			error={apiErrorMessage(
				new ApiError(404, {
					code: "library_entry_not_updated",
					error: "That title is gone from the library.",
				}),
			)}
		/>
	),
};

// --- Her hooks -------------------------------------------------------------------------------

const HOOKS = {
	hooks: [
		{ on: "session.started", run: "notify-send 'Streaming to my phone'" },
		{
			on: "game.exited",
			webhook: "https://home.example/hooks/anna",
			filter: { app: "steam:1086940" },
		},
	],
};

/** Her own hooks, saved with no password: they run as her. */
export const Automation: Story = {
	render: () => (
		<Framed path="/automation" page="automation">
			<Seeded data={[[getGetHooksQueryKey(), HOOKS]]}>
				<AutomationHere />
			</Seeded>
		</Framed>
	),
};

/** A save the host refused, in its own words. */
export const AutomationRefused: Story = {
	render: () => {
		const Refused: FC = () => {
			useEffect(() => {
				toast.error(
					apiErrorMessage(
						new ApiError(500, {
							code: "hooks_not_saved",
							error: "Couldn't save the hooks",
						}),
					) ?? "",
					{ id: "hooks-refused", duration: Number.POSITIVE_INFINITY },
				);
				return () => toast.dismiss("hooks-refused");
			}, []);
			return <Toaster />;
		};
		return (
			<Framed path="/automation" page="automation">
				<Seeded data={[[getGetHooksQueryKey(), HOOKS]]}>
					<AutomationHere />
				</Seeded>
				<Refused />
			</Framed>
		);
	},
};

// --- Her sources: stream settings, shared sets, emulators ------------------------------------

/** Her view of the sources; Lutris she turned off herself. */
const SOURCES: ScannerInfo[] = [
	{
		id: "steam",
		label: "Steam",
		enabled: true,
		origin: "plugin",
		provider: "steam",
		entries: 214,
	},
	{
		id: "heroic",
		label: "Heroic",
		enabled: true,
		origin: "plugin",
		provider: "heroic",
		entries: 40,
	},
	{
		id: "lutris",
		label: "Lutris",
		enabled: false,
		origin: "plugin",
		provider: "lutris",
		entries: 12,
		off_by: "profile",
	},
];

/** Her overrides: Heroic on one of Enrico's shared sets, and the sets the box lends her. */
const HER_OVERRIDES = {
	version: 1,
	sources: { heroic: { set: "9e8d7c6b5a4f" } },
	games: { "steam:1245620": { set: "a1b2c3d4e5f6" } },
	box_sets: overridesDoc.sets,
};

const EMULATORS: EmulatorStatus[] = [
	{
		id: "rpcs3",
		name: "RPCS3",
		home: "/home/anna/.local/share/punktfunk/emulators/rpcs3",
		platforms: ["PS3"],
		offered: true,
		detected: [],
		managed: {
			channel: "stable",
			exe: "/home/anna/.local/share/punktfunk/emulators/rpcs3/rpcs3",
			installed_at: "2026-11-01T10:00:00Z",
			version: "0.0.34",
		},
	},
	{
		id: "pcsx2",
		name: "PCSX2",
		home: "/home/anna/.local/share/punktfunk/emulators/pcsx2",
		platforms: ["PS2"],
		offered: true,
		detected: [],
	},
	{
		id: "dolphin",
		name: "Dolphin",
		home: "/home/anna/.local/share/punktfunk/emulators/dolphin",
		platforms: ["GameCube", "Wii"],
		offered: true,
		detected: [{ exe: "/usr/bin/dolphin-emu", kind: "system" }],
	},
];

/** The Sources tab of `me`'s library, its queries seeded. */
const SourcesPage: FC<{ me?: PersonMe; children?: ReactNode }> = ({
	me = ANNA,
	children,
}) => (
	<Framed path="/library" page="library" me={me}>
		<Seeded
			data={[
				[getGetEmulatorsQueryKey(), EMULATORS],
				[getGetLibraryOverridesQueryKey({ as: me.profile }), HER_OVERRIDES],
			]}
		>
			<div className="flex flex-col gap-card">
				<LibraryHeader />
				<Tabs value="sources">
					<TabsList>
						<TabsTrigger value="games">{m.library_tab_games()}</TabsTrigger>
						<TabsTrigger value="sources">{m.library_tab_sources()}</TabsTrigger>
					</TabsList>
					<TabsContent value="sources">
						<Card>
							<CardContent className="space-y-8">
								<SourcesCard
									sources={SOURCES}
									available={[]}
									running={new Set()}
									busyId={null}
									installBusy={false}
									activeFilter={null}
									onToggle={noop}
									onFilter={noop}
									onSettings={noop}
									onInstall={noop}
									profile={{
										name: me.name,
										ownerName: me.owner ?? "",
										person: true,
									}}
								/>
								<EmulatorsCard />
							</CardContent>
						</Card>
					</TabsContent>
				</Tabs>
				{children}
			</div>
		</Seeded>
	</Framed>
);

/** Her sources: each one's stream settings open from its row, and emulators she installs. */
export const Sources: Story = { render: () => <SourcesPage /> };

/** A source's stream settings, hers alone: Heroic on one of Enrico's shared sets. */
export const SourceStreamSettings: Story = {
	render: () => (
		<SourcesPage>
			<SourceSettingsDialog
				source={{ id: "heroic", label: "Heroic", provider: "heroic" }}
				stream
				form={false}
				onClose={noop}
			/>
		</SourcesPage>
	),
};

/** Oma's own account: her emulators from copies she has, and no Plugins page in her frame. */
export const OwnAccountNoPlugins: Story = {
	render: () => <SourcesPage me={OMA} />,
};

/** Oma points at a copy of her own: no password, her seat host takes it as her. */
export const EmulatorsAdopt: Story = {
	render: () => <SourcesPage me={OMA} />,
	play: async ({ canvasElement }) => {
		const page = within(canvasElement);
		// On a phone the source rows fold their actions into menus too: take RPCS3's.
		const menus = await page.findAllByRole("button", {
			name: m.common_more_actions(),
		});
		const row = (b: HTMLElement) => {
			let el = b.parentElement;
			while (el && !el.textContent?.includes("RPCS3")) el = el.parentElement;
			return el;
		};
		const more = menus.find((b) => !row(b)?.textContent?.includes("Steam"));
		if (more) await userEvent.click(more);
		await userEvent.click(
			await within(document.body).findByRole("menuitem", {
				name: m.emulators_adopt(),
			}),
		);
		const path = await page.findByRole("textbox", {
			name: m.emulators_adopt_path(),
		});
		await userEvent.type(path, "/home/oma/Apps/RPCS3.AppImage");
		path.scrollIntoView({ block: "center" });
	},
};

/** A game's stream settings, choosing among Enrico's shared sets; none of them edits here. */
export const SharedSetChoice: Story = {
	render: () => {
		const elden = hers.find(
			(g) => g.id === "steam:1245620",
		) as OperatorGameEntry;
		const form = formFrom(elden);
		return (
			<Framed path={`/library/${elden.id}`} page="library">
				<Seeded
					data={[
						[
							getGetLibraryOverridesQueryKey({ as: ANNA.profile }),
							HER_OVERRIDES,
						],
					]}
				>
					<EntryView
						entry={elden}
						draft={form}
						baseline={form}
						readOnly
						set={noop}
						tab="launch"
						onTab={noop}
						header={{
							storeName: "Steam",
							managedBy: null,
							who: ANNA.name,
							dirty: false,
							saving: false,
							gated: false,
							password: "",
							onPassword: noop,
							deleting: false,
							onToggleHidden: noop,
							hiding: false,
						}}
					/>
				</Seeded>
			</Framed>
		);
	},
	play: async ({ canvasElement }) => {
		const use = await within(canvasElement).findByRole("combobox", {
			name: m.library_stream_use(),
		});
		use.focus();
		await userEvent.keyboard("{ArrowDown}");
		const menu = await within(document.body).findByRole("listbox");
		await Promise.all(menu.getAnimations().map((a) => a.finished));
	},
};

// --- Her plugins, on the desktop the box made for her -----------------------------------------

const RUNTIME: RuntimeView = {
	installed: true,
	enabled: true,
	running: true,
	unit: "punktfunk-scripting.service",
};

const plugin = (
	id: string,
	title: string,
	categories: string[],
	extra: Partial<CatalogEntry> = {},
): CatalogEntry => ({
	id,
	pkg: `@punktfunk/plugin-${id}`,
	title,
	description: `${title}, from the unom catalog.`,
	icon: "gamepad-2",
	author: "unom",
	version: "0.2.0",
	source: "unom",
	tier: "verified",
	platforms: ["linux"],
	compatible: true,
	update_available: false,
	categories,
	...extra,
});

const CATALOG: CatalogResponse = {
	host: { version: "0.46.0", platform: "linux" },
	busy: false,
	sources: [],
	plugins: [
		plugin("heroic", "Heroic", ["library"], { installed_version: "0.2.0" }),
		plugin("lutris", "Lutris", ["library"]),
		plugin("steamgriddb", "SteamGridDB", ["metadata"]),
		plugin("play-history", "Play history", ["tools"]),
	],
};

const INSTALLED: InstalledView[] = [
	{
		pkg: "@punktfunk/plugin-heroic",
		title: "Heroic",
		version: "0.2.0",
		tier: "verified",
		source: "unom official",
		entry_id: "heroic",
		plugin_id: "heroic",
		running: true,
	},
];

/** Her plugins: the unom catalog, installed on her own desktop, on her session. */
export const Plugins: Story = {
	render: () => (
		<Framed path="/plugins" page="plugins">
			<Seeded
				data={[
					[getGetPluginRuntimeQueryKey(), RUNTIME],
					[getGetPluginCatalogQueryKey(), CATALOG],
					[getListInstalledPluginsQueryKey(), INSTALLED],
					[getListPluginJobsQueryKey(), []],
					[
						getListPluginsQueryKey(),
						[{ id: "heroic", title: "Heroic", category: "library" }],
					],
				]}
			>
				<StoreHere />
			</Seeded>
		</Framed>
	),
};

// --- Settings: the browser's own ---------------------------------------------------------------

/** Language and look; nothing of the box, and the way out stays in the frame. */
export const Settings: Story = {
	render: () => (
		<Framed path="/settings">
			<SectionSettings />
		</Framed>
	),
};

// --- When it closes ----------------------------------------------------------------------------

const closed = (why: Closed): Story => ({
	render: () => <PersonClosed why={why} />,
});

/** A ticket that was bad, used, too old or another seat's. */
export const TicketRefused = closed("refused");
/** Too many refused tickets in a minute. */
export const TicketWait = closed("wait");
/** Her session expired, or her seat host ended it. */
export const SessionEnded = closed("ended");
/** Punktfunk stopped on her computer while the console was open. */
export const SeatStopped = closed("stopped");
/** After Sign out. */
export const SignedOut = closed("signed-out");
