import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { toast } from "@unom/ui/toast";
import { type FC, useState } from "react";
import type { ScannerInfo } from "@/api/gen/model/scannerInfo";
import {
	pluginSurface,
	pluginSurfaceOrThrow,
	refusalText,
} from "@/api/pluginSurface";
import {
	type JsonObject,
	type JsonSchemaDoc,
	SchemaForm,
} from "@/components/schema-form";
import { Button } from "@/components/ui/button";
import {
	Dialog,
	DialogContent,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { Spinner } from "@/components/ui/spinner";
import { m } from "@/paraglide/messages";

/**
 * A library source's settings, rendered as a **generic form** from the plugin's own JSON Schema.
 *
 * A scanner plugin ships no SPA: it serves `GET/PUT /__config` from the kit, and the console renders
 * whatever schema comes back. The read goes through `/api/plugin-config/<id>` on the console origin,
 * which answers JSON read server-side, so the browser never learns the plugin's port or secret and
 * no plugin markup reaches the console origin.
 *
 * Fields the derivation can't express fall back to a raw JSON editor: worst case the drawer is a
 * validated textarea, and the PUT still validates by decode host-side.
 *
 * `config` is optional on the kit's `serveUi`, so a source that ships its own page may serve no
 * `__config` at all. That 404 points at the plugin's page instead of reporting a failure.
 */
export const SourceSettingsDialog: FC<{
	/** A game source, or an Art & Metadata source by its plugin id. */
	source: Pick<ScannerInfo, "id" | "label" | "provider">;
	onClose: () => void;
}> = ({ source, onClose }) => {
	const pluginId = source.provider ?? source.id;
	const configUrl = `/api/plugin-config/${pluginId}`;
	const config = useQuery({
		queryKey: ["plugin-config", pluginId],
		queryFn: async () => {
			const r = await pluginSurface<{
				schema: JsonSchemaDoc | null;
				value: JsonObject | null;
			}>(configUrl);
			if (r.ok) {
				return {
					tag: "ready",
					schema: r.body.schema,
					value: r.body.value ?? {},
				} as const;
			}
			// `config` is optional on the kit's `serveUi`, and a plugin that omits it keeps its
			// settings on the page it already serves. Only the route's own marker means that.
			if (r.status === 404 && r.body?.noConfig === true) {
				return { tag: "ownPage" } as const;
			}
			throw new Error(refusalText(r.body));
		},
		// Each opening reads afresh: the form's draft starts from this load.
		gcTime: 0,
	});
	const [saving, setSaving] = useState(false);

	const save = async (value: JsonObject) => {
		setSaving(true);
		try {
			const saved = await pluginSurfaceOrThrow<{
				access?: { refused?: { path: string; error: string }[] };
			} | null>(configUrl, { method: "PUT", body: value });
			for (const r of saved?.access?.refused ?? []) {
				toast.error(
					m.plugin_form_grant_refused({ path: r.path, issue: r.error }),
				);
			}
			toast.success(m.library_source_settings_saved());
			onClose();
		} catch (e) {
			toast.error(
				m.library_source_settings_failed({
					issue: e instanceof Error ? e.message : String(e),
				}),
			);
		} finally {
			setSaving(false);
		}
	};

	return (
		<Dialog open onOpenChange={(open) => !open && onClose()}>
			<DialogContent>
				<DialogHeader>
					<DialogTitle>
						{m.library_source_settings_title({ source: source.label })}
					</DialogTitle>
				</DialogHeader>
				{config.isPending && <Spinner />}
				{config.isError && !config.data && (
					<p className="text-sm text-destructive">
						{m.library_source_settings_unreachable({
							issue: config.error.message,
						})}
					</p>
				)}
				{config.data?.tag === "ownPage" && (
					<div className="space-y-3">
						<p className="text-sm text-muted-foreground">
							{m.library_source_settings_own_page({ source: source.label })}
						</p>
						<Button asChild onClick={onClose}>
							<Link to="/plugins/$pluginId/$" params={{ pluginId, _splat: "" }}>
								{m.library_source_settings_open_page({ source: source.label })}
							</Link>
						</Button>
					</div>
				)}
				{config.data?.tag === "ready" && (
					<ConfigForm
						schema={config.data.schema}
						value={config.data.value}
						grantee={source.label}
						saving={saving}
						onSave={save}
					/>
				)}
			</DialogContent>
		</Dialog>
	);
};

/** Draft, form, Save. A refused grant is reported per folder; the settings themselves saved. */
const ConfigForm: FC<{
	schema: JsonSchemaDoc | null;
	value: JsonObject;
	grantee: string;
	saving: boolean;
	onSave: (value: JsonObject) => void;
}> = ({ schema, value, grantee, saving, onSave }) => {
	const [draft, setDraft] = useState<JsonObject | null>(value);
	return (
		<div className="space-y-4">
			<SchemaForm
				schema={schema}
				value={draft ?? value}
				onChange={setDraft}
				grantee={grantee}
			/>
			<Button
				disabled={saving || draft === null}
				onClick={() => draft && onSave(draft)}
			>
				{m.library_source_settings_save()}
			</Button>
		</div>
	);
};
