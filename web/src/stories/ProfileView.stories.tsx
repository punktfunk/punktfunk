import type { Meta, StoryObj } from "@storybook/react-vite";
import { useQueryClient } from "@tanstack/react-query";
import { type FC, type ReactNode, useState } from "react";
import { userEvent, within } from "storybook/test";
import { ApiError } from "@/api/fetcher";
import { getGetHooksQueryKey } from "@/api/gen/hooks/hooks";
import { getGetLibraryOverridesQueryKey } from "@/api/gen/library/library";
import type { OperatorGameEntry } from "@/api/gen/model/operatorGameEntry";
import type { ScannerInfo } from "@/api/gen/model/scannerInfo";
import {
	type SeatPage,
	type SeatScopeValue,
	scopeChoices,
	seatClient,
} from "@/api/seat";
import { SeatScopeView } from "@/components/seat-scope";
import { Card, CardContent } from "@/components/ui/card";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { m } from "@/paraglide/messages";
import { AutomationHere } from "@/sections/Automation";
import { LibraryHeader } from "@/sections/Library";
import { formFrom } from "@/sections/Library/Entry/model";
import { EntryView } from "@/sections/Library/Entry/view";
import { LibraryGrid } from "@/sections/Library/LibraryGrid";
import { SourcesCard } from "@/sections/Library/Sources";
import { library, overridesDoc, profilesWindows } from "./lib/fixtures";
import { Routed } from "./lib/routed";
import { Seeded } from "./lib/seeded";

const noop = () => {};
const idle = { isLoading: false, error: null, refetch: noop };

/**
 * A profile's view of the library, and the pages that follow the same pick. The host is a
 * Windows box with every kind of profile: Anna shares Enrico's desktop, Kid has a seat the box
 * made, Oma plays on her own account.
 */
const meta = {
	title: "Pages/Profile view",
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

const ANNA = "0b7d5e2a91c4";
const KID = "9a3f1c2b7e40";
const OMA = "3d5f7b9a1c3e";

/** Page `page` with `id` picked, from the real choice list. */
const scope = (page: SeatPage, id: string | null): SeatScopeValue => {
	const choices = scopeChoices(page, profilesWindows, "windows");
	return {
		page,
		choices,
		current: choices.find((c) => c.id === id) ?? choices[0] ?? null,
		pick: noop,
	};
};

/** A stand-in cover, so the cards read as a library without a host to serve art. */
const cover = (hue: number) =>
	`data:image/svg+xml;utf8,${encodeURIComponent(
		`<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 40 60"><rect width="40" height="60" fill="hsl(${hue} 40% 34%)"/><circle cx="20" cy="24" r="9" fill="hsl(${hue} 55% 68%)"/></svg>`,
	)}`;

/**
 * The library as Anna has it: a shared override from Enrico's library on Elden Ring, her own
 * stream settings on Baldur's Gate 3, and Stardew Valley hidden for her.
 */
const annas: OperatorGameEntry[] = library.map((g, i) => {
	const art = { ...g.art, portrait: cover(i * 67) };
	if (g.id === "steam:1245620")
		return {
			...g,
			art,
			overrides: { name: "1080p60 DualSense", lines: 1080, hz: 60 },
		};
	if (g.id === "steam:1086940")
		return {
			...g,
			art,
			overrides: { lines: 1440, hz: 120, hdr: true },
			profile: { overrides: true },
		};
	if (g.id === "steam:413150") return { ...g, art, hidden: true };
	return { ...g, art };
});

const gridArgs = {
	games: { data: annas, ...idle },
	launchers: [],
	total: annas.length,
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
	deletingId: null,
	onToggleHidden: noop,
	hidingId: null,
	who: "Anna",
};

/** The Library page's frame: the title with its pick, the note, the tabs. */
const Page: FC<{ tab?: "games" | "sources"; children: ReactNode }> = ({
	tab = "games",
	children,
}) => (
	<SeatScopeView {...scope("library", ANNA)}>
		<div className="flex flex-col gap-card">
			<LibraryHeader />
			<Tabs value={tab}>
				<TabsList>
					<TabsTrigger value="games">{m.library_tab_games()}</TabsTrigger>
					<TabsTrigger value="sources">{m.library_tab_sources()}</TabsTrigger>
				</TabsList>
				<TabsContent value={tab} className="flex flex-col gap-card">
					{children}
				</TabsContent>
			</Tabs>
		</div>
	</SeatScopeView>
);

/** Anna's library: what she has hidden and set herself is marked as hers; nothing deletes. */
export const AnnasLibrary: Story = {
	render: () => (
		<Page>
			<LibraryGrid {...gridArgs} />
		</Page>
	),
};

/** The same, as lines. */
export const AnnasLibraryAsList: Story = {
	render: () => (
		<Page>
			<LibraryGrid {...gridArgs} view="rows" />
		</Page>
	),
};

/** Selection mode in Anna's view: *Same as Enrico* hands the titles back to his settings. */
export const AnnasLibrarySelecting: Story = {
	render: () => (
		<Page>
			<LibraryGrid
				{...gridArgs}
				selection={{
					active: true,
					ids: new Set(["steam:1086940", "custom:retroarch"]),
					onActive: noop,
					onToggle: noop,
					sets: [
						{ id: "a1b2c3d4e5f6", name: "1080p60 DualSense" },
						{ id: "0f1e2d3c4b5a", name: "4K" },
					],
					sameAs: m.profile_view_same_as_owner({ owner: "Enrico" }),
					onApply: noop,
					busy: false,
				}}
			/>
		</Page>
	),
};

/** Her view while it loads. */
export const AnnasLibraryLoading: Story = {
	render: () => (
		<Page>
			<LibraryGrid
				{...gridArgs}
				games={{ data: undefined, isLoading: true, error: null }}
			/>
		</Page>
	),
};

/** Her view when the host refuses it: the profile was removed meanwhile. */
export const AnnasLibraryFailed: Story = {
	render: () => (
		<Page>
			<LibraryGrid
				{...gridArgs}
				games={{
					data: undefined,
					isLoading: false,
					error: new ApiError(404, {
						code: "profile_unknown",
						error: "That profile is gone from this host. Pick another one.",
					}),
					refetch: noop,
				}}
			/>
		</Page>
	),
};

/** Nothing listed for her: every source is off. */
export const AnnasLibraryEmpty: Story = {
	render: () => (
		<Page>
			<LibraryGrid
				{...gridArgs}
				games={{ data: [], ...idle }}
				total={0}
				onSources={noop}
			/>
		</Page>
	),
};

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
		id: "lutris",
		label: "Lutris",
		enabled: false,
		origin: "plugin",
		provider: "lutris",
		entries: 12,
		off_by: "profile",
	},
	{
		id: "heroic",
		label: "Heroic",
		enabled: false,
		origin: "plugin",
		provider: "heroic",
		entries: 40,
		off_by: "box",
	},
];

/**
 * Anna's sources: Lutris she turned off herself, and can turn on again; Heroic Enrico's library
 * turned off, so it has a lock and the reason instead of a switch. Nothing installs here.
 */
export const AnnasSources: Story = {
	render: () => (
		<Page tab="sources">
			<Card>
				<CardContent className="space-y-8">
					<SourcesCard
						sources={SOURCES}
						available={[]}
						running={new Set(["steam", "lutris", "heroic"])}
						busyId={null}
						installBusy={false}
						activeFilter={null}
						onToggle={noop}
						onFilter={noop}
						onSettings={noop}
						onInstall={noop}
						profile={{ name: "Anna", ownerName: "Enrico" }}
					/>
				</CardContent>
			</Card>
		</Page>
	),
};

/** The title, opened: Enrico first, then every profile, whatever kind. */
export const PickList: Story = {
	render: () => (
		<SeatScopeView {...scope("library", ANNA)}>
			<div className="min-h-96">
				<LibraryHeader />
			</div>
		</SeatScopeView>
	),
	play: async ({ canvasElement }) => {
		const title = await within(canvasElement).findByRole("combobox", {
			name: m.seat_whose_library(),
		});
		title.focus();
		await userEvent.keyboard("{ArrowDown}");
		// The menu fades in; the story ends once it is opaque.
		const menu = await within(document.body).findByRole("listbox");
		await Promise.all(menu.getAnimations().map((a) => a.finished));
	},
};

/** Enrico's library with other profiles on the box: the pick, and every shared control. */
export const OwnersLibrary: Story = {
	render: () => (
		<SeatScopeView {...scope("library", null)}>
			<div className="flex flex-col gap-card">
				<LibraryHeader onShared={noop} />
				<LibraryGrid
					{...gridArgs}
					games={{
						data: annas.map((g) => {
							const { profile: _, ...rest } = g;
							return { ...rest, hidden: false };
						}),
						...idle,
					}}
					onDelete={noop}
					who={undefined}
				/>
			</div>
		</SeatScopeView>
	),
};

// --- The entry page in Anna's view -------------------------------------------------------

const ownDoc = {
	version: 1,
	games: { "steam:1086940": { lines: 1440, hz: 120, hdr: true } },
};

/** The entry page as Anna's: the views of an entry seeded, with no host behind them. */
const Entry: FC<{ entry: OperatorGameEntry; tab: string }> = ({
	entry,
	tab,
}) => {
	const initial = formFrom(entry);
	return (
		<SeatScopeView {...scope("library", ANNA)}>
			<Seeded
				data={[
					[getGetLibraryOverridesQueryKey(), overridesDoc],
					[getGetLibraryOverridesQueryKey({ as: ANNA }), ownDoc],
				]}
			>
				<EntryView
					entry={entry}
					draft={initial}
					baseline={initial}
					readOnly
					set={noop}
					tab={tab}
					onTab={noop}
					header={{
						storeName: "Steam",
						managedBy: null,
						dirty: false,
						saving: false,
						gated: false,
						password: "",
						onPassword: noop,
						deleting: false,
						onToggleHidden: noop,
						hiding: false,
						who: "Anna",
					}}
				/>
			</Seeded>
		</SeatScopeView>
	);
};

const bg3 = annas.find((g) => g.id === "steam:1086940") as OperatorGameEntry;
const elden = annas.find((g) => g.id === "steam:1245620") as OperatorGameEntry;

/** Stream settings Anna set herself: marked, with one action back to Enrico's. */
export const AnnasStreamSettings: Story = {
	render: () => <Entry entry={bg3} tab="launch" />,
};

/** A title on Enrico's settings: *Same as Enrico*, read muted until she picks her own. */
export const AnnasStreamShared: Story = {
	render: () => <Entry entry={elden} tab="launch" />,
};

/** Anna's own cover on a title she hid: both marked as hers, each with its way back. */
export const AnnasArtAndHidden: Story = {
	render: () => (
		<Entry
			entry={{
				...bg3,
				hidden: true,
				art: { ...bg3.art, hero: cover(200) },
				filled: { portrait: "pick", hero: "steamgriddb" },
				profile: { overrides: true, picks: ["portrait"] },
			}}
			tab="media"
		/>
	),
};

// --- Automation, following the same pick --------------------------------------------------

/**
 * Automation as profile `id` has it. Its seat's cache answers with `hooks`, or runs `queryFn`.
 * Each story's seat id differs, since a seat's cache lives for the session.
 */
const Automation: FC<{
	id: string;
	seat?: string;
	hooks?: unknown;
	queryFn?: () => Promise<unknown>;
}> = ({ id, seat = id, hooks, queryFn }) => {
	const root = useQueryClient();
	const value = scope("automation", id);
	const current = value.current && {
		...value.current,
		seat: value.current.seat && { ...value.current.seat, id: seat },
	};
	useState(() => {
		const qc = seatClient(seat, root);
		if (queryFn) qc.setQueryDefaults(getGetHooksQueryKey(), { queryFn });
		if (hooks) {
			qc.setQueryDefaults(getGetHooksQueryKey(), { staleTime: Infinity });
			qc.setQueryData(getGetHooksQueryKey(), hooks);
		}
		return null;
	});
	return (
		<SeatScopeView {...value} current={current}>
			<AutomationHere />
		</SeatScopeView>
	);
};

/** Kid's seat, which the box made: its hooks, through its own host. */
export const AutomationMadeSeat: Story = {
	render: () => (
		<Automation
			id={KID}
			seat={`${KID}a`}
			hooks={{
				hooks: [
					{ on: "session.started", run: "notify-send 'Kid is playing'" },
					{ on: "game.exited", webhook: "https://home.example/hooks/kid" },
				],
			}}
		/>
	),
};

/** Oma plays on her own account: a sentence stands in for the hooks, no form. */
export const AutomationOwnAccount: Story = {
	render: () => <Automation id={OMA} />,
};

/** Kid's hooks on their way. */
export const AutomationLoading: Story = {
	render: () => (
		<Automation
			id={KID}
			seat={`${KID}b`}
			queryFn={() => new Promise(() => {})}
		/>
	),
};

/** Kid's desktop is stopped, so its host can't answer. */
export const AutomationSeatStopped: Story = {
	render: () => (
		<Automation
			id={KID}
			seat={`${KID}c`}
			queryFn={() =>
				Promise.reject(
					new ApiError(502, {
						code: "seat_no_answer_start",
						error: "The seat didn't answer. Start it, then try again.",
					}),
				)
			}
		/>
	),
};
