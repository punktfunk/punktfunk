import Section from "@unom/ui/section";
import type { FC, ReactNode } from "react";
import type { OperatorGameEntry } from "@/api/gen/model/operatorGameEntry";
import { useProfileView } from "@/api/seat";
import { QueryState } from "@/components/query-state";
import { Button } from "@/components/ui/button";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { apiErrorMessage } from "@/lib/errors";
import { m } from "@/paraglide/messages";
import { InstallPanel } from "../Install";
import { SharedNote } from "../SharedNote";
import { DesktopWait, EntryHeader, type EntryHeaderProps } from "./Header";
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

/**
 * The entry page without its queries: header, then the tab strip. A profile's view says the
 * title is the owner's library's, and offers no install; a game only that profile has needs no
 * such line. While a save waits for the profile's desktop the fields hold still.
 */
export const EntryView: FC<EntryViewProps> = ({
	header,
	tab,
	onTab,
	pluginTabs = [],
	...tabProps
}) => {
	const view = useProfileView();
	const plugins = pluginTabs.filter(
		(p) => !(BUILTIN_TABS as readonly string[]).includes(p.id),
	);
	const known = [...BUILTIN_TABS, ...plugins.map((p) => p.id)];
	const active = known.includes(tab) ? tab : "information";
	return (
		<Section maxWidth={false}>
			<div className="flex flex-col gap-card">
				<EntryHeader
					{...header}
					entry={tabProps.entry}
					title={tabProps.draft.title}
				/>
				{!header.own && <SharedNote />}
				{!view && <InstallPanel entry={tabProps.entry} />}
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
					<fieldset disabled={!!header.waiting} className="contents">
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
					</fieldset>
				</Tabs>
			</div>
		</Section>
	);
};

const noop = () => {};

/**
 * A game only `who` has, before its desktop answered: what the box lists of it, then the wait, or
 * why the desktop didn't start and Retry. The header stands where the opened page has it.
 */
export const OwnEntryWait: FC<{
	listed: OperatorGameEntry | undefined;
	who: string;
	waiting: boolean;
	error: unknown;
	onRetry: () => void;
}> = ({ listed, who, waiting, error, onRetry }) => (
	<Section maxWidth={false}>
		<div className="flex flex-col gap-card">
			{listed && (
				<EntryHeader
					entry={listed}
					title={listed.title}
					storeName={null}
					managedBy={null}
					dirty={false}
					saving={false}
					gated={false}
					password=""
					onPassword={noop}
					deleting={false}
					hiding={false}
					who={who}
					own
				/>
			)}
			{error ? (
				<div
					role="alert"
					className="flex flex-wrap items-center gap-x-3 gap-y-2 rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm"
				>
					<p className="min-w-0 flex-1 text-destructive">
						{apiErrorMessage(error) ?? m.common_error()}
					</p>
					<Button variant="outline" size="sm" onClick={onRetry}>
						{m.common_retry()}
					</Button>
				</div>
			) : waiting ? (
				<DesktopWait line={m.profile_view_desktop_wait_open({ name: who })} />
			) : (
				<QueryState isLoading error={null}>
					{null}
				</QueryState>
			)}
		</div>
	</Section>
);
