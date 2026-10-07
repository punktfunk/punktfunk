import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { ArrowUpCircle } from "lucide-react";
import { type FC, type ReactNode, useState } from "react";
import { ApiError, apiFetch } from "@/api/fetcher";
import type { UpdateStatus } from "@/api/gen/model";
import {
	getGetUpdateStatusQueryKey,
	useForceUpdateCheck,
	useGetUpdateStatus,
} from "@/api/gen/update/update";
import {
	PasswordConfirmField,
	usePasswordFailure,
} from "@/components/password-confirm";
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
import { Spinner } from "@/components/ui/spinner";
import { apiErrorMessage } from "@/lib/errors";
import { fmtDateTimeSecs } from "@/lib/format";
import type { Loadable } from "@/lib/query";
import { m } from "@/paraglide/messages";

/** Give up waiting for the host to come back this long after apply started. */
const APPLY_TIMEOUT_MS = 8 * 60 * 1000;

/**
 * The host update: check everywhere (U0), one-click apply where the host reports `apply: "full"`
 * (U1 — Windows installer). The apply flow survives the console's own backends dying: once an
 * apply is accepted, the card renders from the LAST status snapshot (React Query keeps data
 * across failed polls) and treats poll errors as "the host is restarting", until the target
 * version answers or a timeout.
 */
export function useUpdate() {
	const qc = useQueryClient();
	const [applying, setApplying] = useState<{
		target: string;
		startedAt: number;
	} | null>(null);

	const status = useGetUpdateStatus({
		// Poll fast while an apply is in flight so the post-restart status lands promptly.
		query: { refetchInterval: applying ? 3_000 : 60_000 },
	});
	const check = useForceUpdateCheck();

	const s = status.data;
	// The apply resolved: either the host answers with the target version (reconcile wrote
	// last_result across the restart), or ANY apply outcome newer than our start arrived —
	// which covers failure, "staged, reboot to finish", and "your package source had nothing
	// newer yet" (where the version deliberately doesn't change).
	if (applying && s) {
		if (
			s.current_version === applying.target ||
			(s.last_result &&
				s.last_result.finished_unix * 1000 >= applying.startedAt - 60_000)
		) {
			setApplying(null);
		}
	}

	const checkNow = () =>
		check.mutate(undefined, {
			onSuccess: (fresh) => {
				qc.setQueryData(getGetUpdateStatusQueryKey(), fresh);
			},
			// The host throttles repeat checks (429). Swallowing it made a second click within the
			// window look like a dead button; say the check was skipped and why.
			onError: (e) =>
				toast.error(
					e instanceof ApiError && e.status === 429
						? m.update_apply_throttled()
						: (apiErrorMessage(e) ?? m.update_error()),
				),
		});

	return {
		state: status,
		onCheck: checkNow,
		checkBusy: check.isPending,
		applying,
		onApplied: (target: string) =>
			setApplying({ target, startedAt: Date.now() }),
		onGiveUp: () => setApplying(null),
	};
}

export type Update = ReturnType<typeof useUpdate>;

/**
 * Whether the update needs more than the Host strip's line: one exists or is running, the
 * check failed, or the last one did not simply succeed.
 */
export function updateNeedsCard(u: Update): boolean {
	const s = u.state.data;
	const r = s?.last_result;
	return Boolean(
		u.applying ||
			s?.job ||
			s?.available ||
			s?.last_error ||
			s?.manifest?.stale ||
			(r && (!r.ok || r.staged)),
	);
}

/** The strip's line: whether this host is current. */
export function updateLine(u: Update): string | undefined {
	const s = u.state.data;
	if (!s) return undefined;
	if (u.applying || s.job) return m.host_updating();
	if (s.available) return m.update_available_badge();
	return s.manifest ? m.host_up_to_date() : undefined;
}

export const UpdateCard: FC<{
	state: Loadable<UpdateStatus>;
	onCheck: () => void;
	checkBusy: boolean;
	applying: { target: string; startedAt: number } | null;
	onApplied: (target: string) => void;
	/** Leave the applying state after a timeout — otherwise the card waits forever. */
	onGiveUp: () => void;
}> = ({ state, onCheck, checkBusy, applying, onApplied, onGiveUp }) => {
	const s = state.data;
	const inFlight = Boolean(applying) || Boolean(s?.job);
	const timedOut =
		applying !== null && Date.now() - applying.startedAt > APPLY_TIMEOUT_MS;
	// Is the snapshot we are rendering still being refreshed? While the host is gone the query
	// keeps its LAST payload — including a `job` that was in progress when it went away — so the
	// timeout warning, gated on `!job`, could never fire in the one case it exists for. A failing
	// poll means the job field describes a host we can no longer see.
	const snapshotStale = Boolean(state.error);
	return (
		<Card>
			<CardHeader className="flex flex-row items-center justify-between">
				<CardTitle className="flex items-center gap-2">
					<ArrowUpCircle className="size-4" />
					{m.update_title()}
				</CardTitle>
				{s?.available && !inFlight && (
					<Badge>{m.update_available_badge()}</Badge>
				)}
			</CardHeader>
			<CardContent className="space-y-4">
				<QueryState
					isLoading={state.isLoading}
					// While an apply restarts the host (and, on Windows, this console server),
					// failed polls are EXPECTED — keep rendering the last snapshot instead of
					// swapping the card for an error box.
					error={inFlight ? undefined : state.error}
					refetch={state.refetch}
				>
					{s && (
						<>
							<dl className="grid grid-cols-1 gap-3">
								<UpdateRow
									label={m.update_current()}
									value={
										<span className="flex items-center gap-2 font-medium">
											{s.current_version}
											<Badge variant="secondary">{s.channel}</Badge>
											<Badge variant="outline" title={m.update_install_kind()}>
												{s.install_kind}
											</Badge>
										</span>
									}
								/>
								<UpdateRow
									label={m.update_latest()}
									value={
										s.manifest ? (
											<span className="flex items-center gap-2 font-medium">
												{s.manifest.version}
												{s.manifest.notes_url && (
													<a
														href={s.manifest.notes_url}
														target="_blank"
														rel="noreferrer"
														className="text-sm font-normal underline underline-offset-2"
													>
														{m.update_notes()}
													</a>
												)}
											</span>
										) : (
											<span className="text-sm text-muted-foreground">
												{s.not_published
													? m.update_none_published()
													: m.update_never_checked()}
											</span>
										)
									}
								/>
							</dl>

							{inFlight ? (
								<ApplyProgress
									status={s}
									reconnecting={Boolean(state.error)}
									timedOut={timedOut}
									snapshotStale={snapshotStale}
									onGiveUp={onGiveUp}
								/>
							) : s.available ? (
								s.apply === "full" || s.apply === "staged" ? (
									<ApplyPanel status={s} onApplied={onApplied} />
								) : (
									<div className="space-y-2 rounded-md border p-4">
										<p className="text-sm">{m.update_how()}</p>
										<CommandLine command={s.channel_hint} />
										{s.opt_in_hint && (
											<div className="space-y-1 border-t pt-2">
												<p className="text-sm text-muted-foreground">
													{m.update_opt_in()}
												</p>
												<CommandLine command={s.opt_in_hint} />
											</div>
										)}
									</div>
								)
							) : (
								s.manifest && (
									<p className="text-sm text-muted-foreground">
										{m.update_up_to_date()}
									</p>
								)
							)}

							{!inFlight && s.last_result && (
								<LastResult result={s.last_result} kind={s.install_kind} />
							)}

							{s.manifest?.stale && (
								<p className="rounded-md border border-amber-500/40 bg-amber-500/10 p-3 text-sm">
									{m.update_stale()}
								</p>
							)}
							{/* An empty channel is a normal state, not a fault: it looks like a
							    404 down at the transport, but it means "nobody has announced a
							    release here yet". Rendering it in the failure style told
							    operators their host was broken when nothing was. The host keeps
							    the two apart (`not_published` is never set alongside
							    `last_error`), so this stays a straight either/or. */}
							{!inFlight && s.not_published && (
								<p className="text-sm text-muted-foreground">
									{m.update_not_published({ channel: s.channel })}
								</p>
							)}
							{!inFlight && s.last_error && (
								<p className="text-sm text-destructive">
									{m.update_error()} — {s.last_error}
								</p>
							)}
							{s.check_disabled ? (
								<p className="text-sm text-muted-foreground">
									{m.update_disabled()}
								</p>
							) : (
								!inFlight && (
									<div className="flex items-center gap-3">
										<Button
											variant="outline"
											size="sm"
											onClick={onCheck}
											disabled={checkBusy}
										>
											{checkBusy ? m.update_checking() : m.update_check_now()}
										</Button>
										{s.last_checked_unix != null && (
											<span className="text-xs text-muted-foreground">
												{m.update_last_checked()}{" "}
												{fmtDateTimeSecs(s.last_checked_unix)}
											</span>
										)}
									</div>
								)
							)}
						</>
					)}
				</QueryState>
			</CardContent>
		</Card>
	);
};

/** The one-click leg: the Update button + its password-confirm dialog. */
const ApplyPanel: FC<{
	status: UpdateStatus;
	onApplied: (target: string) => void;
}> = ({ status, onApplied }) => {
	const [open, setOpen] = useState(false);
	const [password, setPassword] = useState("");
	const refusal = usePasswordFailure();
	const [error, setError] = useState<string | null>(null);
	const [needsForce, setNeedsForce] = useState(false);
	const [busy, setBusy] = useState(false);
	const target = status.manifest?.version ?? "";

	const submit = async (force: boolean) => {
		setBusy(true);
		setError(null);
		refusal.reset();
		try {
			await apiFetch("/api/v1/update/apply", {
				method: "POST",
				headers: { "Content-Type": "application/json" },
				body: JSON.stringify({ password, force }),
			});
			setOpen(false);
			setPassword("");
			onApplied(target);
		} catch (e) {
			if (refusal.classify(e)) return;
			const message = e instanceof ApiError ? apiErrorMessage(e) : undefined;
			if (
				e instanceof ApiError &&
				e.status === 409 &&
				message?.includes("force")
			) {
				// The host refused because a stream is live — escalate to the explicit
				// "drop the stream" confirmation instead of showing a raw error.
				setNeedsForce(true);
			} else {
				setError(message || m.common_error());
			}
		} finally {
			setBusy(false);
		}
	};

	return (
		<div className="space-y-2 rounded-md border p-4">
			<p className="text-sm">{m.update_apply_ready({ version: target })}</p>
			<div className="flex items-center gap-3">
				<Button size="sm" onClick={() => setOpen(true)}>
					{m.update_apply_button()}
				</Button>
				<CommandLine command={status.channel_hint} />
			</div>
			<Dialog
				open={open}
				onOpenChange={(o) => {
					setOpen(o);
					if (!o) {
						setPassword("");
						setError(null);
						refusal.reset();
						setNeedsForce(false);
					}
				}}
			>
				<DialogContent>
					<DialogHeader>
						<DialogTitle>
							{m.update_apply_confirm_title({ version: target })}
						</DialogTitle>
						<DialogDescription>
							{needsForce
								? m.update_apply_force_warning()
								: m.update_apply_confirm_body()}
						</DialogDescription>
					</DialogHeader>
					<form
						className="space-y-3"
						onSubmit={(e) => {
							e.preventDefault();
							void submit(needsForce);
						}}
					>
						<PasswordConfirmField
							id="update-apply-password"
							value={password}
							onChange={setPassword}
							failure={refusal.failure}
							autoFocus
						/>
						{error && <p className="text-sm text-destructive">{error}</p>}
						<DialogFooter>
							<Button
								type="submit"
								variant={needsForce ? "destructive" : "default"}
								disabled={busy || password.length === 0}
							>
								{busy
									? m.update_apply_working()
									: needsForce
										? m.update_apply_force_button()
										: m.update_apply_button()}
							</Button>
						</DialogFooter>
					</form>
				</DialogContent>
			</Dialog>
		</div>
	);
};

/** The in-flight panel: download progress → applying → restarting → reconnecting. */
const ApplyProgress: FC<{
	status: UpdateStatus;
	reconnecting: boolean;
	timedOut: boolean;
	/** The rendered snapshot can no longer be refreshed — its `job` may describe a vanished host. */
	snapshotStale: boolean;
	onGiveUp: () => void;
}> = ({ status, reconnecting, timedOut, snapshotStale, onGiveUp }) => {
	const job = status.job;
	const pct =
		job?.total_bytes && job.total_bytes > 0
			? Math.min(100, Math.round((job.received_bytes / job.total_bytes) * 100))
			: null;
	const stageLabel = (() => {
		if (reconnecting) return m.update_stage_reconnecting();
		switch (job?.stage) {
			case "downloading":
				return pct === null
					? m.update_stage_downloading_indeterminate()
					: m.update_stage_downloading({ pct: String(pct) });
			case "verifying":
				return m.update_stage_verifying();
			case "applying":
				return m.update_stage_applying();
			default:
				return m.update_stage_restarting();
		}
	})();
	return (
		<div className="space-y-3 rounded-md border p-4">
			<div className="flex items-center gap-3">
				<Spinner className="size-4" />
				<p className="text-sm font-medium">
					{m.update_applying_title({
						version: job?.target_version ?? status.manifest?.version ?? "",
					})}
				</p>
			</div>
			<p className="text-sm text-muted-foreground">{stageLabel}</p>
			{job?.stage === "downloading" && pct !== null && (
				<div className="h-1.5 w-full overflow-hidden rounded bg-muted">
					<div
						className="h-full rounded bg-primary transition-all"
						style={{ width: `${pct}%` }}
					/>
				</div>
			)}
			{/* A job we can still SEE progressing (e.g. the Deck's tens-of-minutes source rebuild) is
			    not "timed out" — the warning is for the host being GONE longer than a restart
			    explains. A stale snapshot's job does not count as seeing one. */}
			{timedOut && (!job || snapshotStale) && (
				<div className="space-y-2 rounded-md border border-amber-500/40 bg-amber-500/10 p-3 text-sm">
					<p>{m.update_apply_timeout()}</p>
					{/* Without this the card sits in "applying" forever and the operator cannot even
					    re-check — the state was only ever cleared by a status that never arrives. */}
					<Button variant="outline" size="sm" onClick={onGiveUp}>
						{m.update_apply_give_up()}
					</Button>
				</div>
			)}
		</div>
	);
};

/**
 * Durable outcome of the last apply (written by the host across its own restart).
 *
 * Equal versions mean the apply changed nothing. A source build says that in its own
 * words: it has no package source to be behind, and its version carries the commit,
 * so equal really does mean the checkout never moved.
 */
const LastResult: FC<{
	result: NonNullable<UpdateStatus["last_result"]>;
	kind: string;
}> = ({ result, kind }) =>
	result.ok ? (
		<p className="rounded-md border border-emerald-500/40 bg-emerald-500/10 p-3 text-sm">
			{result.staged
				? m.update_result_staged({ to: result.to })
				: result.from === result.to
					? kind === "steamos-source"
						? m.update_result_noop_source()
						: m.update_result_noop()
					: m.update_result_ok({ from: result.from, to: result.to })}
		</p>
	) : (
		<div className="space-y-1 rounded-md border border-destructive/40 bg-destructive/5 p-3 text-sm">
			<p className="font-medium text-destructive">
				{m.update_result_failed({ to: result.to, stage: result.stage ?? "?" })}
			</p>
			{result.error && <p>{result.error}</p>}
			{result.log_path && (
				<p className="text-xs text-muted-foreground">
					{m.update_result_log()} <code>{result.log_path}</code>
				</p>
			)}
		</div>
	);

const UpdateRow: FC<{ label: string; value: ReactNode }> = ({
	label,
	value,
}) => (
	<div className="flex items-baseline justify-between gap-4">
		<dt className="text-sm text-muted-foreground">{label}</dt>
		<dd>{value}</dd>
	</div>
);

/** The copy-pastable update command, with a small clipboard affordance. */
const CommandLine: FC<{ command: string }> = ({ command }) => {
	const [copied, setCopied] = useState(false);
	const copy = () => {
		navigator.clipboard
			.writeText(command)
			.then(() => {
				setCopied(true);
				setTimeout(() => setCopied(false), 2000);
			})
			.catch(() => {
				/* clipboard denied — the text is selectable, nothing to do */
			});
	};
	return (
		<div className="flex min-w-0 flex-1 items-center gap-2">
			<code className="min-w-0 flex-1 overflow-x-auto rounded bg-muted px-2 py-1.5 text-xs">
				{command}
			</code>
			<Button variant="ghost" size="sm" onClick={copy}>
				{copied ? m.update_copied() : m.update_copy()}
			</Button>
		</div>
	);
};
