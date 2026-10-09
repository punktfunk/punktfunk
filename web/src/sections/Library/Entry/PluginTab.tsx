import { useQueries, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { toast } from "@unom/ui/toast";
import { ExternalLink } from "lucide-react";
import { type FC, useState } from "react";
import type { PluginSummary } from "@/api/gen/model";
import {
	pluginSurface,
	pluginSurfaceOrThrow,
	refusalText,
} from "@/api/pluginSurface";
import { gamePlugins, usePlugins } from "@/api/plugins";
import { useSeat } from "@/api/seat";
import {
	type JsonObject,
	type JsonSchemaDoc,
	SchemaForm,
} from "@/components/schema-form";
import { Button } from "@/components/ui/button";
import { m } from "@/paraglide/messages";
import { Group } from "./fields";
import type { PluginTabSpec } from "./view";

interface StatusLine {
	level: "info" | "warn";
	text: string;
}

interface Section {
	schema: JsonSchemaDoc | null;
	value: JsonObject | null;
	status?: StatusLine[];
	/** The entry's route in the plugin's own page. */
	page?: string;
}

/** One segment the plugin route carries; the kit drops anything else, and so does this. */
const PAGE_RE = /^[A-Za-z0-9._~-]{1,200}$/;

type Loaded =
	| { tag: "ready"; section: Section }
	| { tag: "none" }
	| { tag: "offline"; issue: string };

const sectionUrl = (plugin: string, entry: string) =>
	`/api/plugin-game/${plugin}?entry=${encodeURIComponent(entry)}`;

const sectionKey = (plugin: string, entry: string) => [
	"plugin-game",
	plugin,
	entry,
];

async function loadSection(plugin: string, entry: string): Promise<Loaded> {
	const r = await pluginSurface<Section | null>(sectionUrl(plugin, entry));
	if (r.ok) {
		return { tag: "ready", section: r.body ?? { schema: null, value: {} } };
	}
	if (r.status === 404 && r.body?.noSection) return { tag: "none" };
	return { tag: "offline", issue: refusalText(r.body) };
}

/** A tab per plugin with a section for this entry. A plugin that answers "none" adds no tab. */
export function usePluginTabs(entryId: string | null): PluginTabSpec[] {
	const { data } = usePlugins();
	const seat = useSeat();
	// A plugin's section is read from the box's own plugin runner.
	const plugins = entryId && !seat ? gamePlugins(data) : [];
	const results = useQueries({
		queries: plugins.map((p) => ({
			queryKey: sectionKey(p.id, entryId ?? ""),
			queryFn: () => loadSection(p.id, entryId ?? ""),
		})),
	});
	return plugins.flatMap((p, i) => {
		const loaded = results[i]?.data;
		if (!entryId || !loaded || loaded.tag === "none") return [];
		return [
			{
				id: p.id,
				title: p.title,
				panel: <PluginTab plugin={p} entryId={entryId} loaded={loaded} />,
			},
		];
	});
}

const PluginTab: FC<{
	plugin: PluginSummary;
	entryId: string;
	loaded: Exclude<Loaded, { tag: "none" }>;
}> = ({ plugin, entryId, loaded }) => {
	const qc = useQueryClient();
	const stored = loaded.tag === "ready" ? (loaded.section.value ?? {}) : {};
	const [draft, setDraft] = useState<JsonObject | null>(stored);
	const [saving, setSaving] = useState(false);
	const [refused, setRefused] = useState<{ path: string; error: string }[]>([]);

	if (loaded.tag === "offline") {
		return (
			<Group title={plugin.title}>
				<p className="text-sm text-destructive">
					{m.library_entry_plugin_offline({
						plugin: plugin.title,
						issue: loaded.issue,
					})}
				</p>
			</Group>
		);
	}

	const dirty = JSON.stringify(draft) !== JSON.stringify(stored);
	const save = async () => {
		if (!draft) return;
		setSaving(true);
		try {
			const saved = await pluginSurfaceOrThrow<{
				access?: { refused?: { path: string; error: string }[] };
			} | null>(sectionUrl(plugin.id, entryId), { method: "PUT", body: draft });
			setRefused(saved?.access?.refused ?? []);
			toast.success(m.library_entry_saved());
			await qc.invalidateQueries({ queryKey: sectionKey(plugin.id, entryId) });
		} catch (e) {
			toast.error(
				m.library_entry_plugin_save_failed({
					plugin: plugin.title,
					issue: e instanceof Error ? e.message : String(e),
				}),
			);
		} finally {
			setSaving(false);
		}
	};

	const status = loaded.section.status ?? [];
	const page = loaded.section.page;
	return (
		<Group title={plugin.title}>
			{page && PAGE_RE.test(page) && (
				<div>
					<Button asChild size="sm" variant="outline">
						<Link
							to="/plugins/$pluginId/$"
							params={{ pluginId: plugin.id, _splat: page }}
						>
							<ExternalLink className="size-3.5" />
							{m.library_entry_plugin_open({ plugin: plugin.title })}
						</Link>
					</Button>
				</div>
			)}
			{status.length > 0 && (
				<ul className="space-y-1">
					{status.map((line) => (
						<li
							key={`${line.level}:${line.text}`}
							className={`rounded-md border px-3 py-2 text-sm ${
								line.level === "warn"
									? "border-amber-500/40 bg-amber-500/10"
									: "bg-muted/40"
							}`}
						>
							{line.text}
						</li>
					))}
				</ul>
			)}
			<SchemaForm
				schema={loaded.section.schema}
				value={draft ?? stored}
				onChange={setDraft}
				grantee={plugin.title}
			/>
			{refused.map((r) => (
				<p
					key={r.path}
					role="alert"
					className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive"
				>
					{m.plugin_form_grant_refused({ path: r.path, issue: r.error })}
				</p>
			))}
			<div>
				<Button
					size="sm"
					disabled={!dirty || saving || draft === null}
					onClick={save}
				>
					{m.library_save()}
				</Button>
			</div>
		</Group>
	);
};
