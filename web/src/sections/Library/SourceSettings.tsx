import { Link } from "@tanstack/react-router";
import { toast } from "@unom/ui/toast";
import { type FC, useEffect, useState } from "react";
import type { ScannerInfo } from "@/api/gen/model/scannerInfo";
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

interface RefusalBody {
	/** The plugin's own `__config` decode issue. */
	issue?: string;
	/** The route's own refusal: unreachable plugin, bad id, a reply that was not JSON. */
	error?: string;
	/** The route saw the plugin decline a `__config` surface. Only it sets this. */
	noConfig?: boolean;
}

/** Read a refusal body once; a `Response` body cannot be consumed twice. */
const refusalBody = async (res: Response): Promise<RefusalBody | null> =>
	(await res.json().catch(() => null)) as RefusalBody | null;

/** Both shapes carry the reason under a different key — on `issue` alone every transport
 *  failure reads as "the host said no". */
const refusalText = (body: RefusalBody | null): string =>
	body?.issue ?? body?.error ?? m.library_source_settings_refused();

/**
 * A library source's settings, rendered as a **generic form** from the plugin's own JSON Schema.
 *
 * The point (design D7, closing G8): a scanner plugin ships no SPA at all. It serves
 * `GET/PUT /__config` from the kit, and the console renders whatever schema comes back. The browser
 * never learns the plugin's port or secret — the console reads it server-side over loopback.
 *
 * That read goes through `/api/plugin-config/<id>` on the CONSOLE origin, not the `/plugin-ui/…`
 * proxy this used to call. Plugin UIs live on their own origin (2026-08-05 review H-3) and the
 * console origin now answers 404 for `/plugin-ui/**` by design, which broke this drawer for every
 * library plugin — it is the one consumer of that path that is not an iframe. What it needs is
 * DATA, not an embedded UI, so it gets JSON same-origin and no plugin markup ever reaches the
 * console origin.
 *
 * Fields the derivation can't express fall back to a raw JSON editor. That fallback is what bounds
 * the risk of the whole approach: worst case the drawer is a validated textarea, and the PUT still
 * validates by decode host-side either way.
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
	const [state, setState] = useState<
		| { tag: "loading" }
		| { tag: "error"; message: string }
		// The plugin serves no `__config`: its settings are its own page, not this form.
		| { tag: "ownPage" }
		| { tag: "ready"; schema: JsonSchemaDoc | null; value: JsonObject }
	>({ tag: "loading" });
	const [saving, setSaving] = useState(false);

	useEffect(() => {
		let cancelled = false;
		(async () => {
			try {
				const res = await fetch(`/api/plugin-config/${pluginId}`, {
					credentials: "same-origin",
				});
				if (!res.ok) {
					const body = await refusalBody(res);
					// `config` is optional on the kit's `serveUi`, and a plugin that omits it keeps
					// its settings on the page it already serves. Not a failure, so it must not read
					// as one — but only the route's own marker may mean it, never a bare 404.
					if (res.status === 404 && body?.noConfig === true) {
						if (!cancelled) setState({ tag: "ownPage" });
						return;
					}
					throw new Error(refusalText(body));
				}
				const body = (await res.json()) as {
					schema: JsonSchemaDoc | null;
					value: JsonObject | null;
				};
				if (cancelled) return;
				setState({
					tag: "ready",
					schema: body.schema,
					value: body.value ?? {},
				});
			} catch (e) {
				if (!cancelled) {
					setState({
						tag: "error",
						message: e instanceof Error ? e.message : String(e),
					});
				}
			}
		})();
		return () => {
			cancelled = true;
		};
	}, [pluginId]);

	const save = async (value: JsonObject) => {
		setSaving(true);
		try {
			const res = await fetch(`/api/plugin-config/${pluginId}`, {
				method: "PUT",
				credentials: "same-origin",
				headers: { "content-type": "application/json" },
				body: JSON.stringify(value),
			});
			if (!res.ok) throw new Error(refusalText(await refusalBody(res)));
			const saved = (await res.json().catch(() => null)) as {
				access?: { refused?: { path: string; error: string }[] };
			} | null;
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
				{state.tag === "loading" && <Spinner />}
				{state.tag === "error" && (
					<p className="text-sm text-destructive">
						{m.library_source_settings_unreachable({ issue: state.message })}
					</p>
				)}
				{state.tag === "ownPage" && (
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
				{state.tag === "ready" && (
					<ConfigForm
						schema={state.schema}
						value={state.value}
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
