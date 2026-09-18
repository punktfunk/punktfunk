import { type FC, useEffect, useMemo, useState } from "react";
import { useListPairedClients } from "@/api/gen/clients/clients";
import { useGetLibrary } from "@/api/gen/library/library";
import type { HookEntry } from "@/api/gen/model/hookEntry";
import { useListNativeClients } from "@/api/gen/native/native";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Combobox, type ComboboxOption } from "@/components/ui/combobox";
import {
	Dialog,
	DialogContent,
	DialogDescription,
	DialogFooter,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { InputNumber } from "@/components/ui/input-number";
import { Label } from "@/components/ui/label";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { EVENT_KINDS, eventKindLabel } from "@/lib/event-kinds";
import { m } from "@/paraglide/messages";

const EMPTY: HookEntry = { on: "session.started", run: "" };

// Radix reserves the empty string, so "no device filter" needs a value of its own.
const ANY_DEVICE = "any";

/**
 * Add or edit one hook.
 *
 * A hook is either a shell command or a webhook — never both in this form, because "run this AND
 * post that" is two hooks and pretending otherwise makes the failure modes impossible to reason
 * about. The action kind is therefore a choice, not two optional fields.
 */
export const HookForm: FC<{
	/** The hook being edited, `EMPTY`-seeded for a new one, or null when closed. */
	value: HookEntry | null;
	onCancel: () => void;
	onSave: (hook: HookEntry) => void;
}> = ({ value, onCancel, onSave }) => {
	const [draft, setDraft] = useState<HookEntry>(EMPTY);
	const [kind, setKind] = useState<"run" | "webhook">("run");
	const [filtered, setFiltered] = useState(false);

	// Re-seed whenever a different hook is opened (the dialog stays mounted between edits).
	useEffect(() => {
		if (!value) return;
		setDraft(value);
		setKind(value.webhook ? "webhook" : "run");
		setFiltered(!!value.filter);
	}, [value]);

	const set = (patch: Partial<HookEntry>) =>
		setDraft((d) => ({ ...d, ...patch }));

	// Only while the filter section is open: a hook that does not filter has no reason to pull
	// a library that can run to five figures.
	const library = useGetLibrary(undefined, { query: { enabled: filtered } });
	const clients = useListNativeClients({ query: { enabled: filtered } });
	const moonlight = useListPairedClients({ query: { enabled: filtered } });
	const appOptions: ComboboxOption[] = useMemo(
		() =>
			(library.data ?? []).map((g) => ({
				value: g.id,
				label: g.title,
				// Portrait first, header second — the same step-down the library grid does, so a
				// title with only a wide banner still shows its face here.
				image: g.art?.portrait ?? g.art?.header ?? null,
				// Keeps the row height even for a title that ships no art at all.
				fallback: (
					<span className="text-xs font-medium text-muted-foreground">
						{g.title.slice(0, 1).toUpperCase()}
					</span>
				),
			})),
		[library.data],
	);
	// Both planes' paired devices, keyed by the certificate — a device name is neither unique
	// nor fixed, so renaming one would otherwise stop its hooks matching. The name is what the
	// operator reads; the fingerprint is what gets stored.
	const deviceOptions = useMemo(
		() => [
			...(clients.data ?? []).map((c) => ({
				fingerprint: c.fingerprint,
				name: c.name,
			})),
			...(moonlight.data ?? []).map((c) => ({
				fingerprint: c.fingerprint,
				name: c.label ?? c.fingerprint.slice(0, 8),
			})),
		],
		[clients.data, moonlight.data],
	);
	// A hook written before device filters keyed on the certificate. Keep its name selectable,
	// so editing anything else here cannot quietly drop the filter.
	const legacyClient = draft.filter?.fingerprint ? null : draft.filter?.client;

	const action = kind === "run" ? (draft.run ?? "") : (draft.webhook ?? "");
	const ready = draft.on.trim().length > 0 && action.trim().length > 0;

	const commit = () => {
		// Emit exactly one action field, and drop an unticked filter entirely — leaving `{}` behind
		// would read as "filter on nothing" to anyone reading the config file later.
		const out: HookEntry = {
			on: draft.on.trim(),
			...(kind === "run"
				? { run: action.trim(), webhook: null }
				: { webhook: action.trim(), run: null }),
			...(filtered && draft.filter ? { filter: draft.filter } : {}),
			...(draft.debounce_ms ? { debounce_ms: draft.debounce_ms } : {}),
			...(draft.timeout_s ? { timeout_s: draft.timeout_s } : {}),
			...(kind === "webhook" && draft.hmac_secret_file
				? { hmac_secret_file: draft.hmac_secret_file }
				: {}),
		};
		onSave(out);
	};

	return (
		<Dialog open={value !== null} onOpenChange={(o) => !o && onCancel()}>
			<DialogContent className="max-h-[85vh] max-w-xl overflow-y-auto">
				<DialogHeader>
					<DialogTitle>{m.automation_hook_title()}</DialogTitle>
					<DialogDescription>{m.automation_hook_help()}</DialogDescription>
				</DialogHeader>

				<div className="space-y-2">
					<Label htmlFor="hook-on">{m.automation_field_on()}</Label>
					{/* Both, not one. The identifier is what gets written to the config file and what
					    the SSE `?kinds=` filter takes, so it stays in the host's own spelling — but
					    on its own it asked the operator to already know the vocabulary. The name
					    comes from the same table the activity feed labels its rows with. */}
					<Select value={draft.on} onValueChange={(on) => set({ on })}>
						<SelectTrigger id="hook-on">
							<SelectValue />
						</SelectTrigger>
						<SelectContent>
							{EVENT_KINDS.map((k) => (
								<SelectItem key={k} value={k}>
									<span className="flex w-full items-center justify-between gap-4">
										{eventKindLabel(k)}
										<span className="font-mono text-xs text-muted-foreground">
											{k}
										</span>
									</span>
								</SelectItem>
							))}
						</SelectContent>
					</Select>
					<p className="text-xs text-muted-foreground">
						{m.automation_field_on_help()}
					</p>
				</div>

				<fieldset className="space-y-2">
					<legend className="text-sm font-medium">
						{m.automation_field_action()}
					</legend>
					<div className="flex gap-2">
						{(["run", "webhook"] as const).map((k) => (
							<Button
								key={k}
								type="button"
								size="sm"
								variant={kind === k ? "default" : "outline"}
								aria-pressed={kind === k}
								onClick={() => setKind(k)}
							>
								{k === "run"
									? m.automation_action_run()
									: m.automation_action_webhook()}
							</Button>
						))}
					</div>
					{/* A shell command, dressed as one: the prompt marks where the line starts, the
					    monospace makes a path with a typo in it look wrong, and the correction
					    attributes stop a phone capitalising `/usr` or "fixing" a flag. A URL gets
					    none of that — it is prose to the browser and reads better unstyled. */}
					<div className="relative">
						{kind === "run" && (
							<span
								aria-hidden
								className="pointer-events-none absolute left-3 top-1/2 -translate-y-1/2 select-none font-mono text-sm text-muted-foreground"
							>
								$
							</span>
						)}
						<Input
							id="hook-action"
							aria-label={m.automation_field_action()}
							autoComplete="off"
							autoCapitalize="off"
							autoCorrect="off"
							spellCheck={false}
							className={kind === "run" ? "pl-7 font-mono text-sm" : undefined}
							value={action}
							placeholder={
								kind === "run"
									? '/usr/local/bin/on-stream.sh "$PF_EVENT_CLIENT_NAME"'
									: "https://…"
							}
							onChange={(e) =>
								set(
									kind === "run"
										? { run: e.target.value }
										: { webhook: e.target.value },
								)
							}
						/>
					</div>
					<p className="text-xs text-muted-foreground">
						{kind === "run"
							? m.automation_action_run_help()
							: m.automation_action_webhook_help()}
					</p>
				</fieldset>

				{kind === "webhook" && (
					<div className="space-y-2">
						<Label htmlFor="hook-hmac">{m.automation_field_hmac()}</Label>
						<Input
							id="hook-hmac"
							autoComplete="off"
							spellCheck={false}
							value={draft.hmac_secret_file ?? ""}
							onChange={(e) => set({ hmac_secret_file: e.target.value })}
						/>
						<p className="text-xs text-muted-foreground">
							{m.automation_field_hmac_help()}
						</p>
					</div>
				)}

				<Label className="flex items-start gap-3 text-sm font-normal">
					<Checkbox
						checked={filtered}
						onCheckedChange={(n) => setFiltered(n === true)}
						className="mt-0.5"
					/>
					<span>{m.automation_field_filter()}</span>
				</Label>

				{filtered && (
					<div className="grid gap-3 sm:grid-cols-2">
						<div className="space-y-2">
							<Label htmlFor="hook-client">
								{m.automation_filter_client()}
							</Label>
							<Select
								value={draft.filter?.fingerprint ?? legacyClient ?? ANY_DEVICE}
								onValueChange={(v) =>
									set({
										filter: {
											...draft.filter,
											// One device handle, never both: picking here replaces a
											// name a previous version of the console stored.
											client: undefined,
											fingerprint: v === ANY_DEVICE ? undefined : v,
										},
									})
								}
							>
								<SelectTrigger id="hook-client">
									<SelectValue />
								</SelectTrigger>
								<SelectContent>
									<SelectItem value={ANY_DEVICE}>
										{m.automation_filter_none()}
									</SelectItem>
									{legacyClient && (
										<SelectItem value={legacyClient}>{legacyClient}</SelectItem>
									)}
									{deviceOptions.map((d) => (
										<SelectItem key={d.fingerprint} value={d.fingerprint}>
											{d.name}
										</SelectItem>
									))}
								</SelectContent>
							</Select>
						</div>
						<div className="space-y-2">
							<Label htmlFor="hook-app">{m.automation_filter_app()}</Label>
							{/* The event carries the store-qualified id (`steam:570`), so that is what
							    lands in the field — with the title beside it, because nobody knows
							    their app ids by heart. */}
							<Combobox
								id="hook-app"
								options={appOptions}
								empty={m.automation_filter_none()}
								value={draft.filter?.app ?? ""}
								onChange={(app) => set({ filter: { ...draft.filter, app } })}
							/>
							<p className="text-xs text-muted-foreground">
								{m.automation_filter_app_help()}
							</p>
						</div>
					</div>
				)}

				<div className="grid gap-3 sm:grid-cols-2">
					<div className="space-y-2">
						<Label htmlFor="hook-debounce">
							{m.automation_field_debounce()}
						</Label>
						<InputNumber
							id="hook-debounce"
							min={0}
							value={draft.debounce_ms ?? 0}
							onChange={(debounce_ms) => set({ debounce_ms })}
						/>
					</div>
					{kind === "run" && (
						<div className="space-y-2">
							<Label htmlFor="hook-timeout">
								{m.automation_field_timeout()}
							</Label>
							{/* 600 is the host's own ceiling — the old field let 900 through, and the
							    hook then failed at run time rather than at the point of typing it. */}
							<InputNumber
								id="hook-timeout"
								min={1}
								max={600}
								value={draft.timeout_s ?? 30}
								onChange={(timeout_s) => set({ timeout_s })}
							/>
						</div>
					)}
				</div>

				<DialogFooter>
					<Button variant="outline" onClick={onCancel}>
						{m.common_cancel()}
					</Button>
					<Button disabled={!ready} onClick={commit}>
						{m.automation_hook_save()}
					</Button>
				</DialogFooter>
			</DialogContent>
		</Dialog>
	);
};
