import type { Meta, StoryObj } from "@storybook/react-vite";
import { Toaster, toast } from "@unom/ui/toast";
import { type FC, type ReactNode, useEffect, useState } from "react";
import { getGetLibraryOverridesQueryKey } from "@/api/gen/library/library";
import type { CustomEntry } from "@/api/gen/model/customEntry";
import type { OperatorGameEntry } from "@/api/gen/model/operatorGameEntry";
import { type SeatScopeValue, scopeChoices } from "@/api/seat";
import { SeatScopeView } from "@/components/seat-scope";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { m } from "@/paraglide/messages";
import { LibraryHeader } from "@/sections/Library";
import { DesktopError } from "@/sections/Library/desktop";
import type { EntryHeaderProps } from "@/sections/Library/Entry/Header";
import {
	emptyForm,
	type FormState,
	formFromStored,
} from "@/sections/Library/Entry/model";
import { EntryView, OwnEntryWait } from "@/sections/Library/Entry/view";
import {
	LibraryGrid,
	type LibraryGridProps,
} from "@/sections/Library/LibraryGrid";
import { library, overridesDoc, profilesWindows } from "./lib/fixtures";
import { Routed } from "./lib/routed";
import { Seeded } from "./lib/seeded";

const noop = () => {};

/**
 * Games only one profile has. The host is a Windows box: Kid has a desktop the box made, so Kid
 * can have games of their own; Anna shares Enrico's desktop and Oma plays on her own account, so
 * neither gets the action.
 */
const meta = {
	title: "Pages/Profile view/Own games",
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

const KID = "9a3f1c2b7e40";
const ANNA = "0b7d5e2a91c4";
const OMA = "3d5f7b9a1c3e";

/** The Library page's pick, set to `id`, from the real choice list. */
const scope = (id: string): SeatScopeValue => {
	const choices = scopeChoices("library", profilesWindows, "windows");
	return {
		page: "library",
		choices,
		current: choices.find((c) => c.id === id) ?? null,
		pick: noop,
	};
};

/** A stand-in cover, so the cards read as a library without a host to serve art. */
const cover = (hue: number) =>
	`data:image/svg+xml;utf8,${encodeURIComponent(
		`<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 40 60"><rect width="40" height="60" fill="hsl(${hue} 40% 34%)"/><circle cx="20" cy="24" r="9" fill="hsl(${hue} 55% 68%)"/></svg>`,
	)}`;

const MINECRAFT = "own:3f9a0c1b2d4e";
const SCRATCH = "own:7c2e9b4a1f60";

/** Kid's own Minecraft, as the box lists it: a remote cover the box learned. */
const minecraft: OperatorGameEntry = {
	id: MINECRAFT,
	store: "custom",
	title: "Minecraft",
	art: { portrait: cover(110), hero: cover(130) },
	platform: "PC",
	developer: "Mojang",
	own: true,
};

/** Kid's own Scratch, with a cover on Kid's desktop that the box never learns. */
const scratch: OperatorGameEntry = {
	id: SCRATCH,
	store: "custom",
	title: "Scratch projects",
	art: {},
	own: true,
};

/** Kid's library: the shared titles, and Kid's own two among them. */
const kids: OperatorGameEntry[] = [
	...library.map((g, i) => ({
		...g,
		art: { ...g.art, portrait: cover(i * 67) },
	})),
	minecraft,
	scratch,
].sort((a, b) => a.title.localeCompare(b.title));

const gridArgs: LibraryGridProps = {
	games: { data: kids, isLoading: false, error: null, refetch: noop },
	launchers: [],
	total: kids.length,
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
	who: "Kid",
};

/** The Library page's frame for `id`: the title with its pick, the note, the tabs. */
const Page: FC<{ id: string; children: ReactNode }> = ({ id, children }) => (
	<SeatScopeView {...scope(id)}>
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
	</SeatScopeView>
);

// --- The shelf --------------------------------------------------------------------------------

/**
 * Kid's library: *Add game for Kid*, the note that says where each kind of game is added, and
 * Kid's own games marked *Only for Kid*. Scratch has no picture the box knows: the card's title
 * stands in.
 */
export const Shelf: Story = {
	render: () => (
		<Page id={KID}>
			<LibraryGrid {...gridArgs} />
		</Page>
	),
};

/** The same, as lines: Kid's mark stands where a game's source does, and Delete where Hide. */
export const ShelfAsList: Story = {
	render: () => (
		<Page id={KID}>
			<LibraryGrid {...gridArgs} view="rows" />
		</Page>
	),
};

/** A delete while Kid's desktop starts: the toast says so and can cancel. */
export const ShelfDeleting: Story = {
	render: () => {
		const Toast: FC = () => {
			useEffect(() => {
				toast.loading(m.profile_view_desktop_wait_delete({ name: "Kid" }), {
					id: "own-delete",
					duration: Number.POSITIVE_INFINITY,
					action: { label: m.common_cancel(), onClick: noop },
				});
				return () => toast.dismiss("own-delete");
			}, []);
			return <Toaster />;
		};
		return (
			<Page id={KID}>
				<LibraryGrid {...gridArgs} deletingId={SCRATCH} />
				<Toast />
			</Page>
		);
	},
};

/** Oma plays on her own account: no add action, and the note as before. */
export const OwnAccountNoAdd: Story = {
	render: () => (
		<Page id={OMA}>
			<LibraryGrid
				{...gridArgs}
				games={{ ...gridArgs.games, data: kids.filter((g) => !g.own) }}
				total={kids.length - 2}
				onDelete={undefined}
				who="Oma"
			/>
		</Page>
	),
};

/** Anna shares Enrico's desktop: no add action, and the note as before. */
export const SharedDesktopNoAdd: Story = {
	render: () => (
		<Page id={ANNA}>
			<LibraryGrid
				{...gridArgs}
				games={{ ...gridArgs.games, data: kids.filter((g) => !g.own) }}
				total={kids.length - 2}
				onDelete={undefined}
				who="Anna"
			/>
		</Page>
	),
};

// --- The entry page ---------------------------------------------------------------------------

type HeaderOver = Partial<Omit<EntryHeaderProps, "entry" | "title">>;

/** The entry page in Kid's view, with live fields. */
const Entry: FC<{
	entry: OperatorGameEntry | null;
	initial: FormState;
	draft?: FormState;
	tab?: string;
	header?: HeaderOver;
}> = ({
	entry,
	initial,
	draft: start = initial,
	tab = "information",
	header,
}) => {
	const [draft, setDraft] = useState(start);
	const [shown, setShown] = useState(tab);
	return (
		<SeatScopeView {...scope(KID)}>
			<Seeded
				data={[
					[getGetLibraryOverridesQueryKey(), overridesDoc],
					[getGetLibraryOverridesQueryKey({ as: KID }), { version: 1 }],
				]}
			>
				<EntryView
					entry={entry}
					draft={draft}
					baseline={initial}
					readOnly={false}
					set={(key, value) => setDraft((d) => ({ ...d, [key]: value }))}
					tab={shown}
					onTab={setShown}
					header={{
						storeName: null,
						managedBy: null,
						who: "Kid",
						own: true,
						dirty: JSON.stringify(draft) !== JSON.stringify(initial),
						saving: false,
						gated: false,
						password: "",
						onPassword: noop,
						onSave: noop,
						...(entry ? { onDelete: noop } : {}),
						deleting: false,
						hiding: false,
						...header,
					}}
				/>
			</Seeded>
		</SeatScopeView>
	);
};

/** What Kid's desktop stores for Minecraft: how it starts, too. */
const minecraftRow: CustomEntry = {
	id: MINECRAFT,
	title: "Minecraft",
	art: { portrait: "https://img.example/minecraft.jpg" },
	launch: {
		kind: "command",
		value: '"C:\\XboxGames\\Minecraft Launcher\\Content\\Minecraft.exe"',
	},
	platform: "PC",
	developer: "Mojang",
};

/** A new game, typed and ready to add for Kid. */
const typed: FormState = {
	...emptyForm,
	title: "Minecraft",
	command: '"C:\\XboxGames\\Minecraft Launcher\\Content\\Minecraft.exe"',
	developer: "Mojang",
	platform: "PC",
};

/** `/library/new` in Kid's view: the form marked as Kid's, with no note about shared games. */
export const AddGame: Story = {
	render: () => <Entry entry={null} initial={emptyForm} />,
};

/**
 * Add, while Kid's desktop starts: the form stays filled and holds still, a line says what
 * happens, and Cancel is the way out. Nothing is sent until the desktop answers.
 */
export const AddWaiting: Story = {
	render: () => (
		<Entry
			entry={null}
			initial={emptyForm}
			draft={typed}
			header={{
				gated: true,
				password: "correct horse",
				saving: true,
				waiting: {
					line: m.profile_view_desktop_wait_save({ name: "Kid" }),
					onCancel: noop,
				},
			}}
		/>
	),
};

/** Kid's desktop didn't start: the form is still there, with the host's sentence. */
export const AddStartFailed: Story = {
	render: () => (
		<Entry
			entry={null}
			initial={emptyForm}
			draft={typed}
			header={{
				gated: true,
				password: "correct horse",
				error: "Remote Desktop licensing has run out for this server.",
			}}
		/>
	),
};

/** Kid's own Minecraft, opened: marked as Kid's, Delete and Save, no Hide, no note. */
export const OwnGame: Story = {
	render: () => (
		<Entry entry={minecraft} initial={formFromStored(minecraftRow)} />
	),
};

/**
 * Kid's own Scratch has a cover on Kid's desktop, which the box never lists: with the desktop up,
 * the page shows it through the desktop (a stand-in picture here).
 */
export const OwnGameDesktopCover: Story = {
	render: () => {
		const row: CustomEntry = {
			id: SCRATCH,
			title: "Scratch projects",
			art: { portrait: "C:\\Users\\Kid\\Pictures\\scratch-cover.png" },
			launch: {
				kind: "command",
				value: "C:\\Program Files\\Scratch 3\\Scratch 3.exe",
			},
		};
		return (
			<Entry
				entry={{ ...scratch, art: { portrait: cover(280) } }}
				initial={formFromStored(row)}
				tab="media"
			/>
		);
	},
};

/** An own game with no picture at all: the page's mark stands in, as on the shelf. */
export const OwnGameNoPicture: Story = {
	render: () => (
		<Entry
			entry={scratch}
			initial={formFromStored({ id: SCRATCH, title: "Scratch projects" })}
		/>
	),
};

/** Delete, while Kid's desktop starts. */
export const OwnGameDeleting: Story = {
	render: () => (
		<Entry
			entry={minecraft}
			initial={formFromStored(minecraftRow)}
			header={{
				deleting: true,
				waiting: {
					line: m.profile_view_desktop_wait_delete({ name: "Kid" }),
					onCancel: noop,
				},
			}}
		/>
	),
};

/** Opening Kid's own game while Kid's desktop starts: what the box lists, then the wait. */
export const OwnGameOpening: Story = {
	render: () => (
		<SeatScopeView {...scope(KID)}>
			<OwnEntryWait
				listed={minecraft}
				who="Kid"
				waiting
				error={null}
				onRetry={noop}
			/>
		</SeatScopeView>
	),
};

/** Kid's desktop didn't start: the sentence, and Retry. */
export const OwnGameOpenFailed: Story = {
	render: () => (
		<SeatScopeView {...scope(KID)}>
			<OwnEntryWait
				listed={minecraft}
				who="Kid"
				waiting={false}
				error={
					new DesktopError(m.profile_view_desktop_not_ready({ name: "Kid" }))
				}
				onRetry={noop}
			/>
		</SeatScopeView>
	),
};
