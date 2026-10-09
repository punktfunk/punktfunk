import Section from "@unom/ui/section";
import { toast } from "@unom/ui/toast";
import { type FC, useEffect, useMemo, useState } from "react";
import { ApiError } from "@/api/fetcher";
import type { CatalogEntry, InstalledView, Job } from "@/api/gen/model";
import { useGetPluginCatalog, useUninstallPlugin } from "@/api/gen/store/store";
import {
	catalogEntryFor,
	type InstallBody,
	type PendingUpdate,
	planUpdates,
	runningJob,
	type UpdatePlan,
	useInstalledPlugins,
	useInstallPlugin,
	useStoreJobs,
} from "@/api/store";
import { useDialogs } from "@/components/dialogs";
import { passwordFailure } from "@/components/password-confirm";
import { SeatScope, SeatTitle } from "@/components/seat-scope";
import { Stagger } from "@/components/stagger";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";
import { BrowseTab } from "./Browse";
import {
	InstallDialog,
	SpecInstallDialog,
	UpdateAllDialog,
} from "./InstallDialogs";
import { InstalledTab } from "./Installed";
import { BatchPendingCard, JobProgressSection } from "./JobProgress";
import { SourcesTab } from "./Sources";

type StoreTab = "browse" | "installed" | "sources";

/**
 * An "Update all" run in flight.
 *
 * The host takes one package operation at a time (`409` otherwise — `bun` operations share a
 * lockfile and a `node_modules` tree), so this is a queue the console works through one job at a
 * time, not a fan-out. It carries its own copy of what is left rather than re-deriving it from the
 * catalog between packages: every finished install invalidates the installed list, and a queue that
 * re-derived itself would change shape underneath a run the operator already confirmed.
 */
interface UpdateRun {
	/** Not yet started. The one currently installing has already been taken off the front. */
	queue: PendingUpdate[];
	/** How many have finished successfully — `done + 1` is the step now running. */
	done: number;
	total: number;
}

/**
 * The plugin store: browse a catalog, manage what's installed, and choose which catalogs this host
 * trusts. Each tab owns its own queries; this container owns only what genuinely spans them — the
 * install/uninstall mutations, the confirm dialogs their trust tier dictates, and the job the host
 * hands back (which must stay visible whichever tab you switch to while it runs).
 */
export const SectionStore: FC = () => (
	<SeatScope page="plugins">
		<Store />
	</SeatScope>
);

const Store: FC = () => {
	useLocale();
	const { confirm } = useDialogs();
	const [tab, setTab] = useState<StoreTab>("browse");
	// The catalog entry awaiting its install confirmation, and the raw-spec dialog's open state.
	const [target, setTarget] = useState<CatalogEntry | null>(null);
	const [specOpen, setSpecOpen] = useState(false);
	// The job the host is running for us, if any. Cleared by the operator, not by completion — a
	// finished job's log is the only record of what happened.
	const [jobId, setJobId] = useState<string | null>(null);
	// The plan awaiting its one confirmation, and the run that confirmation started. A SNAPSHOT taken
	// when the button was pressed — the installed list refetches on a timer, and the operator must
	// confirm the list they were shown, not whatever it became while they read it.
	const [updateAllTarget, setUpdateAllTarget] = useState<UpdatePlan | null>(
		null,
	);
	const [run, setRun] = useState<UpdateRun | null>(null);

	const catalog = useGetPluginCatalog();
	// Also queried by the Installed tab; react-query serves both from one fetch. Here it is what
	// "Update all" counts, so the button is right even while that tab has never been opened.
	const installed = useInstalledPlugins();
	const plan = useMemo(
		() => planUpdates(installed.data, catalog.data?.plugins),
		[installed.data, catalog.data],
	);
	// Re-attach to a job that was already running when this page loaded — an install survives a
	// reload on the host side, and losing sight of it left the Install buttons armed against a host
	// that answers 409.
	const jobs = useStoreJobs();
	const orphan = runningJob(jobs.data);
	useEffect(() => {
		// A run owns the job slot while it lasts, and it clears the id between packages. This list is
		// only refetched on a focus or a stale read, so during that gap `orphan` can still be the job
		// that just finished — re-attaching to it would remount the progress card, fire its settle
		// handler a second time, and step the run forward over a package it never installed.
		if (run) return;
		if (orphan && !jobId) setJobId(orphan.id);
	}, [orphan, jobId, run]);
	const install = useInstallPlugin();
	const uninstall = useUninstallPlugin();

	/** Turn a failed 202-request into a message: 409 means the host is busy, not that we're broken. */
	const failed = (e: unknown, fallback: string) =>
		toast.error(
			e instanceof ApiError && e.status === 409 ? m.store_busy() : fallback,
		);

	const start = async (body: InstallBody) => {
		try {
			const { job } = await install.mutateAsync(body);
			setJobId(job);
		} catch (e) {
			failed(e, m.store_install_failed());
		}
	};

	const onConfirmEntry = async (entry: CatalogEntry) => {
		setTarget(null);
		await start({ source: entry.source, id: entry.id });
	};

	const onConfirmSpec = async (spec: string, password: string) => {
		try {
			const { job } = await install.mutateAsync({
				spec,
				accept_unverified: true,
				password,
			});
			setJobId(job);
		} catch (e) {
			// A refused password keeps the dialog open with everything the operator typed still in
			// it; anything else closes it as an ordinary install failure.
			if (passwordFailure(e)) throw e;
			failed(e, m.store_install_failed());
		}
	};

	// An update from the Installed tab installs the CATALOG version — so it goes through the very
	// same tier-appropriate dialog a fresh install would, warning included.
	const onUpdate = (plugin: InstalledView) => {
		const entry = catalogEntryFor(plugin, catalog.data?.plugins);
		if (!entry) {
			toast.error(m.store_update_no_entry());
			return;
		}
		setTarget(entry);
	};

	/**
	 * Start the next install of a run, or finish it when the queue runs dry.
	 *
	 * What is left is threaded through the arguments rather than read from `run`: the caller is a
	 * settle handler that already knows the outcome, and reading state it is itself about to replace
	 * is how a queue skips or repeats an entry.
	 */
	const runNext = async (
		queue: PendingUpdate[],
		done: number,
		total: number,
	) => {
		const [next, ...rest] = queue;
		if (!next) {
			setRun(null);
			toast.success(m.store_update_all_finished({ count: done }));
			return;
		}
		// Let the finished job's card go before asking for the next one: the run's own progress card
		// takes over for the moment in between, so the page never shows "Installed." while the next
		// package is already on its way.
		setJobId(null);
		setRun({ queue: rest, done, total });
		try {
			const { job } = await install.mutateAsync({
				source: next.entry.source,
				id: next.entry.id,
			});
			setJobId(job);
		} catch (e) {
			setRun(null);
			failed(e, m.store_install_failed());
		}
	};

	/**
	 * A job of the run finished.
	 *
	 * A failure ENDS the run. The failed job's card — its phase, its error, its log — is the only
	 * record of what went wrong, and starting the next install would replace it with a fresh
	 * spinner; the operator would be left knowing only that something, somewhere, went wrong. So the
	 * run stops on the evidence and says what it did not get to, which they can retry from the rows.
	 */
	const onJobSettled = (job: Job) => {
		if (!run) return;
		if (job.state !== "done") {
			setRun(null);
			toast.error(
				m.store_update_all_stopped({
					done: run.done,
					left: run.queue.length + 1,
				}),
			);
			return;
		}
		void runNext(run.queue, run.done + 1, run.total);
	};

	const onConfirmUpdateAll = (updates: PendingUpdate[]) => {
		setUpdateAllTarget(null);
		void runNext(updates, 0, updates.length);
	};

	// 1-based, and only while a run is live — this is what turns the install card into "2 of 5".
	const step = run ? { index: run.done + 1, total: run.total } : undefined;

	const onUninstall = async (plugin: InstalledView) => {
		const ok = await confirm({
			title: m.store_uninstall_confirm({ title: plugin.title ?? plugin.pkg }),
			description: m.store_uninstall_body(),
			confirmLabel: m.store_uninstall(),
			destructive: true,
		});
		if (!ok) return;
		try {
			const { job } = await uninstall.mutateAsync({
				data: { pkg: plugin.pkg },
			});
			setJobId(job);
		} catch (e) {
			failed(e, m.store_uninstall_failed());
		}
	};

	return (
		<Section maxWidth={false}>
			<div className="flex flex-col gap-card">
				<div className="flex flex-wrap items-start justify-between gap-4">
					<div className="space-y-1">
						<SeatTitle title={m.store_title()} />
						<p className="text-sm text-muted-foreground">
							{m.store_subtitle()}
						</p>
					</div>
				</div>

				{jobId ? (
					<JobProgressSection
						jobId={jobId}
						onDismiss={() => setJobId(null)}
						onSettled={onJobSettled}
						step={step}
					/>
				) : (
					// No job id yet, but a run is live — the install we just asked for has not come
					// back with one. Only reachable mid-run; a lone install has nothing to show here.
					step && <BatchPendingCard step={step} />
				)}

				<Tabs value={tab} onValueChange={(v) => setTab(v as StoreTab)}>
					<TabsList>
						<TabsTrigger value="browse">{m.store_tab_browse()}</TabsTrigger>
						{/* Browse is the tab this page opens on, so the count has to travel to where the
						    operator already is — otherwise "Update all" is only ever found by someone
						    who went looking for it. */}
						<TabsTrigger value="installed">
							{m.store_tab_installed()}
							{plan.updates.length > 0 && (
								<>
									{/* The digit is shorthand for the sentence beside it; a screen reader
									    gets the sentence, not a tab label that ends in a bare number. */}
									<span
										aria-hidden="true"
										className="ml-2 rounded-full bg-primary px-1.5 py-0.5 text-[0.6875rem] font-medium leading-none tabular-nums text-primary-foreground"
									>
										{plan.updates.length}
									</span>
									<span className="sr-only">
										{m.store_updates_pending({ count: plan.updates.length })}
									</span>
								</>
							)}
						</TabsTrigger>
						<TabsTrigger value="sources">{m.store_tab_sources()}</TabsTrigger>
					</TabsList>

					{/* Each panel drives its own entrance: switching tabs mounts it after the page animated. */}
					<TabsContent value="browse">
						<Stagger root className="contents">
							<BrowseTab
								onInstall={setTarget}
								onInstallSpec={() => setSpecOpen(true)}
							/>
						</Stagger>
					</TabsContent>
					<TabsContent value="installed">
						<Stagger root className="contents">
							<InstalledTab
								onUpdate={onUpdate}
								onUpdateAll={() => setUpdateAllTarget(plan)}
								onUninstall={onUninstall}
								updateCount={plan.updates.length}
								busyPkg={
									uninstall.isPending
										? (uninstall.variables?.data.pkg ?? null)
										: null
								}
								batchRunning={run !== null}
							/>
						</Stagger>
					</TabsContent>
					<TabsContent value="sources">
						<Stagger root className="contents">
							<SourcesTab />
						</Stagger>
					</TabsContent>
				</Tabs>

				<InstallDialog
					entry={target}
					isPending={install.isPending}
					onCancel={() => setTarget(null)}
					onConfirm={onConfirmEntry}
				/>
				<UpdateAllDialog
					updates={updateAllTarget?.updates ?? null}
					skipped={updateAllTarget?.skipped ?? []}
					isPending={install.isPending}
					onCancel={() => setUpdateAllTarget(null)}
					onConfirm={onConfirmUpdateAll}
				/>
				<SpecInstallDialog
					open={specOpen}
					onClose={() => setSpecOpen(false)}
					onConfirm={onConfirmSpec}
				/>
			</div>
		</Section>
	);
};
