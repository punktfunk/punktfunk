import type { Meta, StoryObj } from "@storybook/react-vite";
import { type ReactNode, useState } from "react";
import { getGetLibraryOverridesQueryKey } from "@/api/gen/library/library";
import type { OperatorGameEntry } from "@/api/gen/model/operatorGameEntry";
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

export const ManagedMedia: Story = {
	render: () => {
		const steam = { ...steamGame, provider: "steam" };
		return (
			<Live
				entry={steam}
				initial={formFrom(steam)}
				readOnly
				startTab="media"
				headerOver={{
					storeName: "Steam",
					managedBy: "Steam",
					onSave: undefined,
				}}
			/>
		);
	},
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

const managedHeader = {
	storeName: "Steam",
	managedBy: "Steam",
	onSave: undefined,
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
