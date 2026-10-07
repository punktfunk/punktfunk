// Host → Settings (design/host-settings-console.md §5).
//
// The host sends every setting it acts on, in order; this page groups them, filters them, and
// renders one row each. A setting that host.env or a command-line flag pins is shown locked with
// its source named, never as a control that saves and does nothing.
import Section from "@unom/ui/section";
import { Lock, RotateCcw, Search } from "lucide-react";
import {
	type FC,
	type ReactNode,
	useEffect,
	useMemo,
	useRef,
	useState,
} from "react";
import {
	type HostSettingsState,
	SettingGroup,
	type SettingState,
} from "@/api/gen/model";
import { DocsLink } from "@/components/docs-link";
import { QueryState } from "@/components/query-state";
import { Stagger } from "@/components/stagger";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardTitle } from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import type { Loadable } from "@/lib/query";
import { m } from "@/paraglide/messages";
import { Control, labelOf } from "./controls";
import { GROUP_LABEL, SETTING_COPY } from "./copy";

const ADVANCED_KEY = "pf-host-settings-advanced";

/** Per browser. Read after mount, so the server render and the first client render agree. */
function useAdvanced(): [boolean, (on: boolean) => void] {
	const [on, setOn] = useState(false);
	useEffect(() => {
		try {
			setOn(window.localStorage.getItem(ADVANCED_KEY) === "1");
		} catch {
			// Storage blocked: the switch still works for this visit.
		}
	}, []);
	const set = (next: boolean) => {
		setOn(next);
		try {
			window.localStorage.setItem(ADVANCED_KEY, next ? "1" : "0");
		} catch {
			// As above.
		}
	};
	return [on, set];
}

const matches = (row: SettingState, q: string) =>
	[
		labelOf(row),
		SETTING_COPY[row.id]?.hint?.() ?? "",
		row.id,
		row.env,
		row.origin ?? "",
	].some((s) => s.toLowerCase().includes(q));

const same = (a: unknown, b: unknown) =>
	JSON.stringify(a) === JSON.stringify(b);

const Status: FC<{ row: SettingState }> = ({ row }) => {
	if (row.source === "env" || row.source === "flag") {
		return (
			<p className="flex items-center gap-1.5 text-xs text-muted-foreground">
				<Lock className="size-3 shrink-0" aria-hidden />
				{row.source === "env"
					? m.host_settings_locked_env({ name: row.origin ?? row.env })
					: m.host_settings_locked_flag({ name: row.origin ?? "" })}
			</p>
		);
	}
	if (row.restart_pending) {
		return (
			<p className="text-xs text-amber-600 dark:text-amber-500">
				{m.host_settings_restart_pending()}
			</p>
		);
	}
	if (row.apply === "restart") {
		return (
			<p className="text-xs text-muted-foreground">
				{m.host_settings_applies_restart()}
			</p>
		);
	}
	return null;
};

const SettingRow: FC<{
	row: SettingState;
	busy: boolean;
	onSet: (id: string, value: unknown) => void;
	playingApps?: string[];
}> = ({ row, busy, onSet, playingApps }) => {
	const locked = row.source === "env" || row.source === "flag";
	const hint = SETTING_COPY[row.id]?.hint?.();
	const resettable =
		!locked && row.stored != null && !same(row.stored, row.default);
	return (
		<li className="flex flex-col gap-3 py-4 first:pt-1 last:pb-1 md:flex-row md:items-start md:justify-between md:gap-8">
			<div className="min-w-0 space-y-1 md:max-w-md">
				<div className="flex flex-wrap items-center gap-2">
					<span className="text-sm font-medium">{labelOf(row)}</span>
					{row.advanced && (
						<Badge variant="outline">{m.host_settings_advanced_badge()}</Badge>
					)}
				</div>
				{hint && (
					<p className="text-xs text-muted-foreground">
						{hint}{" "}
						{/* The page links the configuration reference once; a row links only a page of its own. */}
						{row.docs !== "configuration" && <DocsLink path={row.docs} />}
					</p>
				)}
				<Status row={row} />
			</div>
			<div className="flex w-full items-start justify-between gap-1 md:w-auto md:justify-end">
				<div className="min-w-0">
					<Control
						row={row}
						disabled={busy || locked}
						onSet={(v) => onSet(row.id, v)}
						playingApps={playingApps}
					/>
				</div>
				{resettable && (
					<Button
						size="sm"
						variant="ghost"
						title={m.host_settings_reset()}
						aria-label={`${m.host_settings_reset()} ${labelOf(row)}`}
						disabled={busy}
						onClick={() => onSet(row.id, null)}
					>
						<RotateCcw className="size-4" aria-hidden />
					</Button>
				)}
			</div>
		</li>
	);
};

export const HostSettingsView: FC<{
	state: Loadable<HostSettingsState>;
	/** Setting ids with a write in flight. */
	pending: ReadonlySet<string>;
	onSet: (id: string, value: unknown) => void;
	playingApps?: string[];
	/** Above the groups: what waits for a restart. */
	banner?: ReactNode;
	/** Rows another plane serves, at the end of their group (Session's game rows, Video's GPU). */
	extra?: Partial<Record<SettingGroup, ReactNode>>;
	/** Above the settings: who this host is, and what needs doing about its updates. */
	top?: ReactNode;
}> = ({ state, pending, onSet, playingApps, banner, extra = {}, top }) => {
	const [query, setQuery] = useState("");
	const [advanced, setAdvanced] = useAdvanced();
	const q = query.trim().toLowerCase();

	const groups = useMemo(() => {
		const out: { group: SettingGroup; rows: SettingState[] }[] = [];
		for (const row of state.data?.settings ?? []) {
			// A search reaches advanced rows too: an env name typed in is the migration path.
			if (q ? !matches(row, q) : row.advanced && !advanced) continue;
			const g = out.find((it) => it.group === row.group);
			if (g) g.rows.push(row);
			else out.push({ group: row.group, rows: [row] });
		}
		// A search narrows to registry rows; otherwise a group with only extra rows still shows.
		if (!q)
			for (const group of Object.keys(extra) as SettingGroup[])
				if (!out.some((g) => g.group === group)) out.push({ group, rows: [] });
		const order = Object.values(SettingGroup);
		return out.sort((a, b) => order.indexOf(a.group) - order.indexOf(b.group));
	}, [state.data, q, advanced, extra]);

	// One group at a time, named in the URL (`/host#session`). A search shows every match instead.
	const [tab, setTab] = useState("");
	useEffect(() => setTab(window.location.hash.slice(1)), []);
	const active = groups.some((g) => g.group === tab)
		? tab
		: (groups[0]?.group ?? "");
	const pick = (group: string) => {
		setTab(group);
		window.history.replaceState(window.history.state, "", `#${group}`);
	};
	// On a phone the strip scrolls: keep the open tab in sight, a deep link's too.
	const strip = useRef<HTMLDivElement>(null);
	useEffect(() => {
		if (!active) return;
		strip.current
			?.querySelector('[data-state="active"]')
			?.scrollIntoView({ block: "nearest", inline: "nearest" });
	}, [active]);
	// The tab names the group; a search stacks several, so then each card does.
	const card = ({ group, rows }: (typeof groups)[number]) => (
		<Card key={group} id={group}>
			<CardContent className="space-y-2">
				{q && (
					<CardTitle>
						<h2>{GROUP_LABEL[group]()}</h2>
					</CardTitle>
				)}
				<ul className="divide-y">
					{rows.map((row) => (
						<SettingRow
							key={row.id}
							row={row}
							busy={pending.has(row.id)}
							onSet={onSet}
							playingApps={
								row.id === "audio_voice_apps" ? playingApps : undefined
							}
						/>
					))}
					{!q && extra[group]}
				</ul>
			</CardContent>
		</Card>
	);

	return (
		<Section maxWidth={false}>
			<div className="flex flex-col gap-card">
				<div className="flex flex-wrap items-end gap-3">
					<h1 className="text-2xl font-semibold">{m.nav_host()}</h1>
					<div className="flex w-full flex-wrap items-center gap-x-4 gap-y-2 sm:ml-auto sm:w-auto">
						<div className="relative w-full sm:w-64">
							<Search
								className="pointer-events-none absolute top-1/2 left-2.5 size-4 -translate-y-1/2 text-muted-foreground"
								aria-hidden
							/>
							<Input
								type="search"
								className="pl-8"
								placeholder={m.host_settings_search()}
								aria-label={m.host_settings_search()}
								value={query}
								onChange={(e) => setQuery(e.target.value)}
							/>
						</div>
						<div className="flex items-center gap-2">
							<Checkbox
								id="host-settings-advanced"
								checked={advanced}
								onCheckedChange={(v) => setAdvanced(v === true)}
							/>
							<Label
								htmlFor="host-settings-advanced"
								className="font-normal whitespace-nowrap"
							>
								{m.host_settings_advanced()}
							</Label>
						</div>
					</div>
				</div>
				{top}
				<p className="text-sm text-muted-foreground">
					{m.host_settings_intro()}{" "}
					<DocsLink path="configuration#settings-in-the-web-console" />
				</p>
				{banner}

				<QueryState
					isLoading={state.isLoading}
					error={state.error}
					refetch={state.refetch}
				>
					{groups.length === 0 ? (
						<p className="text-sm text-muted-foreground">
							{m.host_settings_no_match({ query: query.trim() })}
						</p>
					) : q ? (
						<Stagger className="flex min-w-0 flex-col gap-card">
							{groups.map(card)}
						</Stagger>
					) : (
						<Tabs
							value={active}
							onValueChange={pick}
							className="flex min-w-0 flex-col gap-card"
						>
							{/* Eight groups outgrow a phone: the strip scrolls, the page does not. */}
							<div
								ref={strip}
								className="-mx-4 overflow-x-auto px-4 sm:mx-0 sm:px-0"
							>
								<TabsList aria-label={m.host_settings_sections()}>
									{groups.map(({ group }) => (
										<TabsTrigger key={group} value={group}>
											{GROUP_LABEL[group]()}
										</TabsTrigger>
									))}
								</TabsList>
							</div>
							{groups.map((g) => (
								<TabsContent key={g.group} value={g.group}>
									{/* A panel mounts after the page animated: it runs its own. */}
									<Stagger root>{card(g)}</Stagger>
								</TabsContent>
							))}
						</Tabs>
					)}
				</QueryState>
			</div>
		</Section>
	);
};
