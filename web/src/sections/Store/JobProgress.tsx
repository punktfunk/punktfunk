import { useQueryClient } from "@tanstack/react-query";
import { CheckCircle2, RotateCw, X, XCircle } from "lucide-react";
import { type FC, useEffect, useRef } from "react";
import type { Job } from "@/api/gen/model";
import { invalidateStore, useStoreJob } from "@/api/store";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Spinner } from "@/components/ui/spinner";
import { m } from "@/paraglide/messages";

// The host reports a job's progress as a free-form phase string; these are the ones it emits, in
// order. Anything unrecognised falls through to the raw phase rather than an empty label, so a new
// host phase degrades to English-but-visible instead of disappearing.
const PHASES: Record<string, () => string> = {
	queued: () => m.store_phase_queued(),
	verifying: () => m.store_phase_verifying(),
	installing: () => m.store_phase_installing(),
	removing: () => m.store_phase_removing(),
	checking: () => m.store_phase_checking(),
	"rolling back": () => m.store_phase_rolling_back(),
	recording: () => m.store_phase_recording(),
	"restarting runner": () => m.store_phase_restarting(),
	done: () => m.store_phase_done(),
};

const phaseLabel = (phase: string): string => PHASES[phase]?.() ?? phase;

/** Keep the tail — an install log can run long and only the end is ever interesting. */
const LOG_TAIL = 200;

/** Where a job sits in an "Update all" run. Absent for a job the operator started on its own. */
export interface BatchStep {
	/** 1-based, so it reads the way it is written: "Update 2 of 5". */
	index: number;
	total: number;
}

/**
 * Container: the in-flight install/uninstall. Polls the job once a second while it runs (the query
 * stops polling itself once the job settles), and refreshes everything the job touched — catalog,
 * installed list, runner state, plugin directory — the moment it finishes.
 */
export const JobProgressSection: FC<{
	jobId: string;
	onDismiss: () => void;
	/**
	 * Called once, with the final job, when it reaches `done` or `failed` — how an Update-all run
	 * learns it may start the next install. The host takes one package operation at a time, so the
	 * run has to be driven by this rather than by a timer.
	 */
	onSettled?: (job: Job) => void;
	step?: BatchStep;
}> = ({ jobId, onDismiss, onSettled, step }) => {
	const qc = useQueryClient();
	const job = useStoreJob(jobId);
	const final =
		job.data?.state === "done" || job.data?.state === "failed"
			? job.data
			: undefined;
	// Refresh once per job, not on every re-render while the finished card sits there. The settle
	// handler starts the next install of a run, so riding the same guard is what keeps a run from
	// double-stepping when a later poll re-renders this with the same finished job.
	const refreshed = useRef<string | null>(null);

	useEffect(() => {
		if (!final || refreshed.current === jobId) return;
		refreshed.current = jobId;
		invalidateStore(qc);
		onSettled?.(final);
	}, [final, jobId, qc, onSettled]);

	// A job the host can no longer tell us about — it restarted, and jobs live in memory. This used
	// to render `null`, so the card simply vanished while the Install buttons stayed armed and the
	// query kept polling a dead id once a second forever. Say what happened and offer the way out.
	if (!job.data) {
		// Mid-run, "no data yet" is just the first poll of the install we only now started, and
		// rendering nothing would blink the run's progress off the page between every package.
		if (!job.isError) return step ? <BatchPendingCard step={step} /> : null;
		return (
			<Card className="ring-2 ring-destructive/60">
				<CardContent className="flex items-start gap-3">
					<XCircle className="mt-0.5 size-5 shrink-0 text-destructive" />
					<div className="min-w-0 flex-1">
						<p className="text-sm font-medium">{m.store_job_lost()}</p>
						<p className="text-sm text-muted-foreground">
							{m.store_job_lost_hint()}
						</p>
					</div>
					<Button
						variant="ghost"
						size="icon"
						aria-label={m.store_job_dismiss()}
						onClick={onDismiss}
					>
						<X className="size-4" />
					</Button>
				</CardContent>
			</Card>
		);
	}
	return <JobProgressCard job={job.data} onDismiss={onDismiss} step={step} />;
};

/**
 * The gap between two installs of a run: the last one finished, the next has not been accepted yet.
 *
 * It exists so the run never appears to stop. Without it the card unmounts the moment the finished
 * job is let go and comes back a request later, which reads as "it gave up" precisely when the
 * operator is watching to see that it hasn't.
 */
export const BatchPendingCard: FC<{ step: BatchStep }> = ({ step }) => (
	<Card aria-live="polite">
		<CardContent className="flex items-start gap-3">
			<Spinner className="mt-0.5 size-5 shrink-0" />
			<div className="min-w-0 flex-1">
				<p className="text-sm font-medium">{m.store_update_all_running()}</p>
				<p className="text-sm text-muted-foreground">
					{m.store_update_all_step({ index: step.index, total: step.total })}
				</p>
			</div>
		</CardContent>
	</Card>
);

/** The progress card: phase (or outcome), a collapsible log tail, and the failure reason if any. */
export const JobProgressCard: FC<{
	job: Job;
	onDismiss: () => void;
	step?: BatchStep;
}> = ({ job, onDismiss, step }) => {
	const running = job.state === "running";
	const failed = job.state === "failed";
	const log = job.log.slice(-LOG_TAIL);

	return (
		<Card
			className={failed ? "ring-2 ring-destructive/60" : undefined}
			aria-live="polite"
		>
			<CardContent className="space-y-3">
				<div className="flex items-start gap-3">
					{running ? (
						<Spinner className="mt-0.5 size-5 shrink-0" />
					) : failed ? (
						<XCircle className="mt-0.5 size-5 shrink-0 text-destructive" />
					) : (
						<CheckCircle2 className="mt-0.5 size-5 shrink-0 text-[var(--success)]" />
					)}
					<div className="min-w-0 flex-1">
						<p className="text-sm font-medium">
							{job.kind === "uninstall"
								? m.store_job_uninstall({ target: job.target })
								: m.store_job_install({ target: job.target })}
						</p>
						{/* Which package is being installed answers "what is happening"; the step answers
						    "how much longer", which is the only question a multi-package run adds. */}
						{step && (
							<p className="text-xs text-muted-foreground">
								{m.store_update_all_step({
									index: step.index,
									total: step.total,
								})}
							</p>
						)}
						<p className="text-sm text-muted-foreground">
							{running
								? phaseLabel(job.phase)
								: failed
									? m.store_job_failed()
									: job.kind === "uninstall"
										? m.store_job_done_uninstall()
										: m.store_job_done_install()}
						</p>
						{running && job.walk && (
							<p className="break-all text-xs text-muted-foreground">
								{m.store_job_walk({
									path: job.walk.path,
									seconds: job.walk.seconds,
								})}
							</p>
						)}
					</div>
					{!running && (
						<Button
							variant="ghost"
							size="icon"
							aria-label={m.store_job_dismiss()}
							onClick={onDismiss}
						>
							<X className="size-4" />
						</Button>
					)}
				</div>

				{failed && job.error && (
					<p className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive">
						{job.error}
					</p>
				)}

				{/* The runner is bounced at the end of every successful job, so the plugin's nav entry
				    (and its UI) takes a moment longer to appear than the job's "done". Say so. */}
				{job.state === "done" && (
					<p className="flex items-center gap-2 text-xs text-muted-foreground">
						<RotateCw className="size-3.5" />
						{m.store_job_restarting()}
					</p>
				)}

				{log.length > 0 && (
					<details className="group">
						<summary className="cursor-pointer list-none text-xs text-muted-foreground transition-colors hover:text-foreground">
							{m.store_job_log()}
						</summary>
						<pre className="mt-2 max-h-64 overflow-auto rounded-md bg-muted p-3 text-left text-xs text-muted-foreground">
							<code>{log.join("\n")}</code>
						</pre>
					</details>
				)}
			</CardContent>
		</Card>
	);
};
