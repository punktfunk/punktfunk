import type { Meta, StoryObj } from "@storybook/react-vite";
import { type ReactNode, useEffect, useState } from "react";
import {
	getGetLibraryOverridesQueryKey,
	getListLibraryMetadataQueryKey,
	getListLibraryScannersQueryKey,
} from "@/api/gen/library/library";
import type { OperatorGameEntry } from "@/api/gen/model/operatorGameEntry";
import { getListPluginsQueryKey } from "@/api/gen/plugins/plugins";
import { type SourceImage, sourceKey } from "@/api/metadata";
import type { EntryHeaderProps } from "@/sections/Library/Entry/Header";
import {
	emptyForm,
	type FormState,
	formFrom,
	formFromStored,
} from "@/sections/Library/Entry/model";
import { EntryView, type PluginTabSpec } from "@/sections/Library/Entry/view";
import { library, overridesDoc } from "./lib/fixtures";
import { Routed } from "./lib/routed";
import { Seeded } from "./lib/seeded";

const noop = () => {};

const custom: OperatorGameEntry = {
	id: "custom:eden",
	store: "custom",
	title: "Eden",
	art: { portrait: null, hero: null, header: null, logo: null },
	launch: { kind: "command", value: "/usr/bin/eden" },
	platform: "Switch",
	developer: "Eden team",
	genres: ["Emulator"],
};

const steamGame: OperatorGameEntry =
	library.find((g) => g.store === "steam") ?? custom;

const header = (
	over: Partial<Omit<EntryHeaderProps, "entry" | "title">> = {},
): Omit<EntryHeaderProps, "entry" | "title"> => ({
	storeName: "Custom",
	managedBy: null,
	dirty: false,
	saving: false,
	gated: false,
	password: "",
	onPassword: noop,
	onSave: noop,
	deleting: false,
	onToggleHidden: noop,
	hiding: false,
	...over,
});

/** The view with live state, so the fields type and the tabs switch. */
const Live = ({
	entry,
	initial,
	readOnly = false,
	headerOver,
	pluginTabs,
	startTab = "information",
}: {
	entry: OperatorGameEntry | null;
	initial: FormState;
	readOnly?: boolean;
	headerOver?: Partial<Omit<EntryHeaderProps, "entry" | "title">>;
	pluginTabs?: PluginTabSpec[];
	startTab?: string;
}) => {
	const [draft, setDraft] = useState(initial);
	const [tab, setTab] = useState(startTab);
	return (
		<EntryView
			entry={entry}
			draft={draft}
			baseline={initial}
			readOnly={readOnly}
			set={(key, value) => setDraft((d) => ({ ...d, [key]: value }))}
			tab={tab}
			onTab={setTab}
			pluginTabs={pluginTabs}
			header={header({
				dirty: JSON.stringify(draft) !== JSON.stringify(initial),
				...headerOver,
			})}
		/>
	);
};

const meta = {
	title: "Pages/Library/Entry",
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

/** A custom entry the operator owns: every tab edits, and a command asks for the password. */
export const Custom: Story = {
	render: () => (
		<Live
			entry={custom}
			initial={formFromStored({
				...custom,
				id: "eden",
				detect: { exe: "/usr/bin/eden", install_dir: null, process_name: null },
				prep: [{ do: "systemctl --user start eden-sync", undo: null }],
			})}
			headerOver={{ gated: true }}
		/>
	),
};

/** A Steam game: the source owns it, the built-in tabs only show what it sent. */
export const Managed: Story = {
	render: () => {
		const steam = { ...steamGame, provider: "steam" };
		return (
			<Live
				entry={steam}
				initial={formFrom(steam)}
				readOnly
				headerOver={{
					storeName: "Steam",
					managedBy: "Steam",
					onSave: undefined,
				}}
			/>
		);
	},
};

/** A drawn picture of `w`×`h`, so the media stories need no network. */
const picture = (w: number, h: number, hue: number, text = ""): string =>
	`data:image/svg+xml,${encodeURIComponent(
		`<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${w} ${h}"><defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="hsl(${hue} 70% 45%)"/><stop offset="1" stop-color="hsl(${hue + 60} 65% 22%)"/></linearGradient></defs><rect width="${w}" height="${h}" fill="url(#g)"/><text x="50%" y="50%" fill="white" font-family="sans-serif" font-weight="700" font-size="${Math.round(Math.min(w, h) / 7)}" text-anchor="middle" dominant-baseline="middle">${text}</text></svg>`,
	)}`;

/** A logo: white lettering on nothing, as most are. */
const logo = (text: string): string =>
	`data:image/svg+xml,${encodeURIComponent(
		`<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 640 360"><text x="50%" y="50%" fill="white" font-family="serif" font-weight="700" font-size="88" text-anchor="middle" dominant-baseline="middle">${text}</text></svg>`,
	)}`;

const offered = (
	n: number,
	w: number,
	h: number,
	styles: string[],
): SourceImage[] =>
	Array.from({ length: n }, (_, i) => {
		const url = picture(w, h, (i * 47) % 360, String(i + 1));
		return {
			url,
			thumb: url,
			label: `${w}×${h} · ${styles[i % styles.length]}`,
			width: w,
			height: h,
		};
	});

const SGDB = "steamgriddb";
const LOGOS: SourceImage[] = ["HALF-LIFE", "Half-Life", "λ", "HL"].map(
	(text) => ({ url: logo(text), thumb: logo(text), label: "official" }),
);

/** Answers the two writes of the Media tab, a pick and a new match, while the story shows. */
const Stored = ({ children }: { children: ReactNode }) => {
	useEffect(() => {
		const real = window.fetch;
		window.fetch = (input, init) => {
			const url = String(input instanceof Request ? input.url : input);
			if (init?.method !== "PUT") return real(input, init);
			const answer = url.includes("/plugin-metadata/")
				? {
						match: { key: "2255", label: "Opposing Force (1999)" },
						pinned: true,
					}
				: {};
			return Promise.resolve(Response.json(answer));
		};
		return () => {
			window.fetch = real;
		};
	}, []);
	return <>{children}</>;
};

/** One running Art & Metadata source that matched `entry`, with images for every slot. */
const withSource = (entry: OperatorGameEntry, node: ReactNode) => (
	<Seeded
		data={[
			[
				getListLibraryMetadataQueryKey(),
				[
					{
						id: SGDB,
						matching: "search",
						enabled: true,
						replace: false,
						entries: 120,
					},
				],
			],
			[
				getListPluginsQueryKey(),
				[{ id: SGDB, title: "SteamGridDB", category: "metadata" }],
			],
			[getListLibraryScannersQueryKey(), []],
			[
				sourceKey(SGDB, "status"),
				{ ready: true, wanted: 120, found: 111, searchable: true },
			],
			[
				sourceKey(SGDB, "match", entry.id),
				{
					match: { key: "2254", label: `${entry.title} (1998)` },
					pinned: false,
				},
			],
			[
				sourceKey(SGDB, "images", entry.id, "portrait"),
				offered(14, 600, 900, ["alternate", "blurred", "material"]),
			],
			[
				sourceKey(SGDB, "images", entry.id, "hero"),
				offered(5, 1920, 620, ["alternate", "blurred"]),
			],
			[
				sourceKey(SGDB, "images", entry.id, "header"),
				offered(7, 920, 430, ["alternate", "white logo"]),
			],
			[sourceKey(SGDB, "images", entry.id, "logo"), LOGOS],
			[
				sourceKey(SGDB, "search", entry.id, entry.title),
				[
					{
						key: "2254",
						label: `${entry.title} (1998)`,
						thumb: picture(600, 900, 20),
					},
					{
						key: "2255",
						label: `${entry.title}: Opposing Force (1999)`,
						thumb: picture(600, 900, 140),
					},
					{
						key: "5248",
						label: `${entry.title} 2: Episode Two — The Orange Box Edition, Remastered Collection (2007)`,
						thumb: picture(600, 900, 260),
					},
					{ key: "9001", label: `${entry.title}: Source`, thumb: null },
				],
			],
		]}
	>
		<Stored>{node}</Stored>
	</Seeded>
);

const steamArt: OperatorGameEntry = {
	...steamGame,
	provider: "steam",
	art: {
		portrait: picture(600, 900, 20, "1"),
		hero: picture(1920, 620, 200, "1"),
		header: picture(920, 430, 300, "1"),
		logo: logo("HALF-LIFE"),
	},
	filled: { portrait: "pick", hero: SGDB, header: SGDB, logo: SGDB },
};

const managedHeader = {
	storeName: "Steam",
	managedBy: "Steam",
	onSave: undefined,
};

/** A managed game with a source running: its match once, and every slot's images below. */
export const ManagedMedia: Story = {
	render: () =>
		withSource(
			steamArt,
			<Live
				entry={steamArt}
				initial={formFrom(steamArt)}
				readOnly
				startTab="media"
				headerOver={managedHeader}
			/>,
		),
};

/** The operator's own entry: a picked image goes into the draft, and saves with it. */
export const CustomMedia: Story = {
	render: () =>
		withSource(
			custom,
			<Live
				entry={custom}
				initial={formFromStored({ ...custom, id: "eden" })}
				startTab="media"
			/>,
		),
};

/** No Art & Metadata source runs: the pictures the entry has, and a pasted address. */
export const MediaWithoutSource: Story = {
	render: () => (
		<Seeded
			data={[
				[getListLibraryMetadataQueryKey(), []],
				[getListPluginsQueryKey(), []],
				[getListLibraryScannersQueryKey(), []],
			]}
		>
			<Live
				entry={steamArt}
				initial={formFrom(steamArt)}
				readOnly
				startTab="media"
				headerOver={managedHeader}
			/>
		</Seeded>
	),
};

/** `/library/new`. */
export const Create: Story = {
	render: () => (
		<Live
			entry={null}
			initial={emptyForm}
			headerOver={{ storeName: null, onToggleHidden: undefined }}
		/>
	),
};

/** The stream settings read the host's overrides document, seeded here. */
const withOverrides = (node: ReactNode) => (
	<Seeded data={[[getGetLibraryOverridesQueryKey(), overridesDoc]]}>
		{node}
	</Seeded>
);

/** A Steam game on a shared override: its values read muted, and change in its dialog. */
export const StreamShared: Story = {
	render: () => {
		const steam = { ...steamGame, provider: "steam" };
		return withOverrides(
			<Live
				entry={steam}
				initial={formFrom(steam)}
				readOnly
				startTab="launch"
				headerOver={managedHeader}
			/>,
		);
	},
};

/** A game with values of its own: every row edits and saves as it changes. */
export const StreamCustom: Story = {
	render: () =>
		withOverrides(
			<Live
				entry={custom}
				initial={formFromStored({
					...custom,
					id: "eden",
					detect: {
						exe: "/usr/bin/eden",
						install_dir: null,
						process_name: null,
					},
				})}
				startTab="launch"
			/>,
		),
};

/** A Lutris title with nothing of its own: the source's values, muted, "from Lutris". */
export const StreamFromSource: Story = {
	render: () => {
		const lutris: OperatorGameEntry = {
			id: "lutris:diablo-ii",
			store: "lutris",
			provider: "lutris",
			title: "Diablo II",
			art: { portrait: null, hero: null, header: null, logo: null },
			launch: { kind: "command", value: "lutris lutris:rungameid/4" },
		};
		return withOverrides(
			<Live
				entry={lutris}
				initial={formFrom(lutris)}
				readOnly
				startTab="launch"
				headerOver={{
					...managedHeader,
					storeName: "Lutris",
					managedBy: "Lutris",
				}}
			/>,
		);
	},
};
