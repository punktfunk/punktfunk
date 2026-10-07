import Section from "@unom/ui/section";
import type { FC, ReactNode } from "react";
import { SeatNote } from "@/components/seat-scope";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { m } from "@/paraglide/messages";
import { InstallPanel } from "../Install";
import { EntryHeader, type EntryHeaderProps } from "./Header";
import { InformationTab } from "./tabs/Information";
import { LaunchTab } from "./tabs/Launch";
import { MediaTab } from "./tabs/Media";
import type { TabProps } from "./tabs/types";

/** The built-in tabs. A plugin whose id is one of these gets no tab. */
export const BUILTIN_TABS = ["information", "media", "launch"] as const;

export interface PluginTabSpec {
	id: string;
	title: string;
	panel: ReactNode;
}

export type EntryViewProps = TabProps & {
	header: Omit<EntryHeaderProps, "entry" | "title">;
	tab: string;
	onTab: (tab: string) => void;
	pluginTabs?: PluginTabSpec[];
};

/** The entry page without its queries: header, then the tab strip. */
export const EntryView: FC<EntryViewProps> = ({
	header,
	tab,
	onTab,
	pluginTabs = [],
	...tabProps
}) => {
	const plugins = pluginTabs.filter(
		(p) => !(BUILTIN_TABS as readonly string[]).includes(p.id),
	);
	const known = [...BUILTIN_TABS, ...plugins.map((p) => p.id)];
	const active = known.includes(tab) ? tab : "information";
	return (
		<Section maxWidth={false}>
			<div className="flex flex-col gap-card">
				<SeatNote />
				<EntryHeader
					{...header}
					entry={tabProps.entry}
					title={tabProps.draft.title}
				/>
				<InstallPanel entry={tabProps.entry} />
				<Tabs value={active} onValueChange={onTab}>
					<div className="-mx-1 overflow-x-auto px-1">
						<TabsList>
							<TabsTrigger value="information">
								{m.library_entry_tab_information()}
							</TabsTrigger>
							<TabsTrigger value="media">
								{m.library_entry_tab_media()}
							</TabsTrigger>
							<TabsTrigger value="launch">
								{m.library_entry_tab_launch()}
							</TabsTrigger>
							{plugins.map((p) => (
								<TabsTrigger key={p.id} value={p.id}>
									{p.title}
								</TabsTrigger>
							))}
						</TabsList>
					</div>
					<TabsContent value="information" className="flex flex-col gap-card">
						<InformationTab {...tabProps} />
					</TabsContent>
					<TabsContent value="media" className="flex flex-col gap-card">
						<MediaTab {...tabProps} />
					</TabsContent>
					<TabsContent value="launch" className="flex flex-col gap-card">
						<LaunchTab {...tabProps} />
					</TabsContent>
					{plugins.map((p) => (
						<TabsContent
							key={p.id}
							value={p.id}
							className="flex flex-col gap-card"
						>
							{p.panel}
						</TabsContent>
					))}
				</Tabs>
			</div>
		</Section>
	);
};
