import Section from "@unom/ui/section";
import { toast } from "@unom/ui/toast";
import { Pencil, Plus, Terminal, Trash2, Webhook, Zap } from "lucide-react";
import { type FC, useEffect, useMemo, useState } from "react";
import { ApiError } from "@/api/fetcher";
import { useListPairedClients } from "@/api/gen/clients/clients";
import { useGetHooks } from "@/api/gen/hooks/hooks";
import type { HookEntry } from "@/api/gen/model/hookEntry";
import { useListNativeClients } from "@/api/gen/native/native";
import { hookAction, hookFilterSummary, useSaveHooks } from "@/api/hooks";
import { useDialogs } from "@/components/dialogs";
import { QueryState } from "@/components/query-state";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
	Dialog,
	DialogContent,
	DialogDescription,
	DialogFooter,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";
import { HookForm } from "./HookForm";

/**
 * **Automation** — the operator's event hooks (`GET/PUT /api/v1/hooks`).
 *
 * The host has run these since the API existed and the console never showed them: the only way to
 * see or change what your machine does when a stream starts was to edit the config file by hand.
 *
 * The whole list is written in one PUT (the host has no per-hook route), so this edits a local copy
 * and saves explicitly — no auto-save. That is deliberate for a screen whose contents are shell
 * commands: a half-typed command should never reach the host because a poll landed.
 */
export const SectionAutomation: FC = () => {
	useLocale();
	const { confirm } = useDialogs();
	const query = useGetHooks();
	const save = useSaveHooks();
	// Filters store the certificate; these two lists are what turn one back into a device name.
	const native = useListNativeClients();
	const moonlight = useListPairedClients();
	const deviceNames = useMemo(
		() =>
			new Map([
				...(native.data ?? []).map((c): [string, string] => [
					c.fingerprint,
					c.name,
				]),
				...(moonlight.data ?? []).map((c): [string, string] => [
					c.fingerprint,
					c.label ?? c.fingerprint.slice(0, 8),
				]),
			]),
		[native.data, moonlight.data],
	);

	const [hooks, setHooks] = useState<HookEntry[] | null>(null);
	const [editing, setEditing] = useState<{
		index: number;
		hook: HookEntry;
	} | null>(null);
	const [confirming, setConfirming] = useState(false);
	const [password, setPassword] = useState("");
	const [wrongPassword, setWrongPassword] = useState(false);

	// Seed once. Unlike the display card there is no re-seed-when-clean dance: nothing else in the
	// console writes hooks, so the server value cannot move underneath an edit.
	const server = query.data?.hooks;
	useEffect(() => {
		if (hooks === null && server) setHooks(server);
	}, [server, hooks]);

	const list = hooks ?? [];
	const dirty =
		hooks !== null && JSON.stringify(hooks) !== JSON.stringify(server ?? []);

	const upsert = (hook: HookEntry) => {
		if (!editing) return;
		setHooks((prev) => {
			const next = [...(prev ?? [])];
			if (editing.index < 0) next.push(hook);
			else next[editing.index] = hook;
			return next;
		});
		setEditing(null);
	};

	const remove = async (index: number) => {
		const ok = await confirm({
			title: m.automation_delete_confirm(),
			confirmLabel: m.automation_delete(),
			destructive: true,
		});
		if (!ok) return;
		setHooks((prev) => (prev ?? []).filter((_, i) => i !== index));
	};

	const commit = async () => {
		setWrongPassword(false);
		try {
			await save.mutateAsync({ hooks: list, password });
			setConfirming(false);
			setPassword("");
			toast.success(m.automation_saved());
		} catch (e) {
			if (e instanceof ApiError && e.status === 401) {
				setWrongPassword(true);
				return;
			}
			toast.error(m.automation_save_failed());
		}
	};

	return (
		<Section maxWidth={false}>
			<div className="flex flex-col gap-card">
				<div className="space-y-1">
					<h1 className="text-2xl font-semibold">{m.automation_title()}</h1>
					<p className="max-w-prose text-sm text-muted-foreground">
						{m.automation_subtitle()}
					</p>
				</div>

				<Card>
					<CardHeader className="flex-row items-center justify-between space-y-0">
						<CardTitle className="flex items-center gap-2">
							<Zap className="size-4" />
							{m.automation_hooks_title()}
						</CardTitle>
						<Button
							size="sm"
							variant="outline"
							onClick={() =>
								setEditing({
									index: -1,
									hook: { on: "session.started", run: "" },
								})
							}
						>
							<Plus className="size-4" />
							{m.automation_add()}
						</Button>
					</CardHeader>
					<CardContent className="space-y-3">
						<QueryState
							isLoading={query.isLoading}
							error={query.error}
							refetch={query.refetch}
						>
							{list.length === 0 ? (
								<p className="text-sm text-muted-foreground">
									{m.automation_empty()}
								</p>
							) : (
								<ul className="flex flex-col gap-2">
									{list.map((h, i) => (
										<li
											// The list is operator-ordered and has no ids; the index IS the identity
											// here, and rows only move when the operator moves them.
											key={`${h.on}:${hookAction(h)}:${i}`}
											className="flex items-start gap-3 rounded-lg border p-3"
										>
											{h.webhook ? (
												<Webhook className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
											) : (
												<Terminal className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
											)}
											<div className="min-w-0 flex-1 space-y-1">
												<div className="flex flex-wrap items-center gap-2">
													<Badge variant="secondary">{h.on}</Badge>
													{hookFilterSummary(h, deviceNames) && (
														<Badge variant="outline">
															{hookFilterSummary(h, deviceNames)}
														</Badge>
													)}
													{!!h.debounce_ms && (
														<Badge variant="outline">
															{m.automation_debounce_badge({
																ms: h.debounce_ms,
															})}
														</Badge>
													)}
												</div>
												<p className="truncate font-mono text-xs text-muted-foreground">
													{hookAction(h)}
												</p>
											</div>
											<Button
												variant="ghost"
												size="icon"
												aria-label={m.automation_edit()}
												onClick={() => setEditing({ index: i, hook: h })}
											>
												<Pencil className="size-4" />
											</Button>
											<Button
												variant="ghost"
												size="icon"
												aria-label={m.automation_delete()}
												onClick={() => remove(i)}
											>
												<Trash2 className="size-4 text-destructive" />
											</Button>
										</li>
									))}
								</ul>
							)}
						</QueryState>

						{dirty && (
							<div className="flex flex-wrap items-center gap-3 rounded-md bg-[var(--warning)]/10 px-3 py-2">
								<span className="text-sm font-medium">
									{m.automation_unsaved()}
								</span>
								<div className="ml-auto flex gap-2">
									<Button
										variant="outline"
										size="sm"
										onClick={() => setHooks(server ?? [])}
									>
										{m.display_revert()}
									</Button>
									<Button size="sm" onClick={() => setConfirming(true)}>
										{m.display_save()}
									</Button>
								</div>
							</div>
						)}
					</CardContent>
				</Card>
			</div>

			<HookForm
				value={editing?.hook ?? null}
				onCancel={() => setEditing(null)}
				onSave={upsert}
			/>

			{/* Saving installs commands the host will run on its own — same bar as an update or an
			    unreviewed install, so the same password. */}
			<Dialog
				open={confirming}
				onOpenChange={(o) => {
					if (!o) {
						setConfirming(false);
						setWrongPassword(false);
					}
				}}
			>
				<DialogContent>
					<DialogHeader>
						<DialogTitle>{m.automation_confirm_title()}</DialogTitle>
						<DialogDescription>{m.automation_confirm_body()}</DialogDescription>
					</DialogHeader>
					<div className="space-y-2">
						<Label htmlFor="automation-password">
							{m.store_spec_password()}
						</Label>
						<Input
							id="automation-password"
							type="password"
							autoComplete="current-password"
							value={password}
							onChange={(e) => setPassword(e.target.value)}
						/>
						{wrongPassword && (
							<p role="alert" className="text-xs text-destructive">
								{m.update_apply_wrong_password()}
							</p>
						)}
					</div>
					<DialogFooter>
						<Button
							variant="outline"
							onClick={() => {
								setConfirming(false);
								setWrongPassword(false);
							}}
						>
							{m.common_cancel()}
						</Button>
						<Button
							disabled={save.isPending || password.length === 0}
							onClick={commit}
						>
							{m.display_save()}
						</Button>
					</DialogFooter>
				</DialogContent>
			</Dialog>
		</Section>
	);
};
