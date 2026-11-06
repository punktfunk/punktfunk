// The plugin store's rules over the generated client (`@/api/gen/store/store`): provenance tiers,
// update planning, job polling and re-attach, cache invalidation, and the two writes that carry the
// console password. Those two stay hand-rolled because the BFF strips a field the spec lacks.
import {
	type QueryClient,
	useMutation,
	useQueryClient,
} from "@tanstack/react-query";
import { apiFetch } from "@/api/fetcher";
import type {
	CatalogEntry,
	InstalledView,
	Job,
	JobRef,
	SourceInput,
} from "@/api/gen/model";
import { getListPluginsQueryKey } from "@/api/gen/plugins/plugins";
import {
	getGetPluginCatalogQueryKey,
	getGetPluginRuntimeQueryKey,
	getListInstalledPluginsQueryKey,
	getListPluginSourcesQueryKey,
	useDeletePluginSource,
	useGetPluginJob,
	useListInstalledPlugins,
	useListPluginJobs,
	useRefreshPluginCatalog,
	useSetPluginRuntime,
} from "@/api/gen/store/store";
import { boostPluginPolling } from "@/api/plugins";
import type { OwnAccount } from "@/api/seat";

/**
 * How much a plugin's provenance is worth, from most to least trustworthy:
 *
 * - `verified` — from the built-in `unom` source; unom reviewed that exact tarball.
 * - `external` — from an operator-added source; pinned and integrity-checked, but curated by
 *   somebody else. It is NOT reviewed by unom and never wears the verified badge.
 * - `unverified` — installed from a raw package spec through the high-friction dialog. No catalog,
 *   no review, no pinning. Stays badged unverified forever.
 * - `cli` — installed with the CLI, so the host holds no provenance record at all.
 */
export type StoreTier = "verified" | "external" | "unverified" | "cli";

/**
 * The catalog entries the page's host installs. A seat on a person's own Linux account installs
 * none and a Windows one only `verified` ones: their hosts refuse the rest.
 */
export function installable(
	entries: CatalogEntry[] | undefined,
	own: OwnAccount | null,
): CatalogEntry[] {
	if (own === "linux") return [];
	const list = entries ?? [];
	return own === "windows"
		? list.filter((e) => e.tier === ("verified" satisfies StoreTier))
		: list;
}

/** An installed plugin paired with the catalog entry an update would install. */
export interface PendingUpdate {
	plugin: InstalledView;
	entry: CatalogEntry;
}

/** What "Update all" would do: the run, and what it deliberately left out of it. */
export interface UpdatePlan {
	/** Ready to install, in the order the run will work through them. */
	updates: PendingUpdate[];
	/** Display names of plugins offering an update this host cannot take right now. */
	skipped: string[];
}

/**
 * The catalog entry an installed plugin updates FROM.
 *
 * Resolve by the entry the plugin was actually installed from (source + entry id) before falling
 * back to the package name: two sources may carry the same `pkg`, and matching on the name alone
 * could offer a row badged "verified" an entry from somebody else's source at a different version.
 */
export function catalogEntryFor(
	plugin: InstalledView,
	entries: CatalogEntry[] | undefined,
): CatalogEntry | undefined {
	const list = entries ?? [];
	return (
		(plugin.source && plugin.entry_id
			? list.find((e) => e.source === plugin.source && e.id === plugin.entry_id)
			: undefined) ??
		(plugin.source
			? list.find((e) => e.source === plugin.source && e.pkg === plugin.pkg)
			: undefined) ??
		list.find((e) => e.pkg === plugin.pkg)
	);
}

/**
 * Everything an "Update all" run would install, in the installed list's own order — so the run
 * follows the rows on screen rather than some order of its own.
 *
 * Two things are dropped rather than attempted, and both are reported instead of hidden: an update
 * with no catalog entry to install (the same dead end a single row's button reports on click), and
 * an entry this host will refuse — incompatible ones are a `400` from `POST /store/install`, and a
 * blocked one is what Browse already greys its Install button out for. Either would end the run on
 * a failure card that says nothing about the updates still queued behind it, so they never enter
 * the queue in the first place.
 *
 * `plugin.blocked` is NOT a reason to skip: that advisory is against the version installed right
 * now, and updating away from it is the fix, not the risk.
 */
export function planUpdates(
	installed: InstalledView[] | undefined,
	entries: CatalogEntry[] | undefined,
): UpdatePlan {
	const plan: UpdatePlan = { updates: [], skipped: [] };
	for (const plugin of installed ?? []) {
		if (plugin.update_available == null) continue;
		const entry = catalogEntryFor(plugin, entries);
		if (entry?.compatible && entry.blocked == null) {
			plan.updates.push({ plugin, entry });
		} else {
			plan.skipped.push(plugin.title ?? plugin.pkg);
		}
	}
	return plan;
}

/**
 * Install a curated catalog entry, or — deliberately awkward — a raw package spec.
 *
 * The raw-spec branch carries the console `password`: it runs unreviewed code, so the BFF
 * re-confirms it (server/routes/api/v1/store/install.post.ts) and strips it before the host ever
 * sees the request. A catalog install needs no password — that trust decision was made when the
 * source was added.
 */
export type InstallBody =
	| { source: string; id: string }
	| { spec: string; accept_unverified: true; password: string };

/** Adding or repointing a source is a trust-root change, so it carries the console password too
 * (stripped at the BFF — server/routes/api/v1/store/sources/[name].put.ts). */
export type SourceBody = SourceInput & { password: string };

const BASE = "/api/v1/store";

const json = (method: string, body: unknown): RequestInit => ({
	method,
	headers: { "Content-Type": "application/json" },
	body: JSON.stringify(body),
});

/**
 * Refresh everything a completed install/uninstall touches — the catalog (installed markers), the
 * installed list, the runner state (it restarts), and the plugin directory the nav is built from.
 */
export function invalidateStore(qc: QueryClient): Promise<void> {
	// The runner restarts AFTER the job reports done, so the plugin registers its UI a few seconds
	// from now — this invalidation would otherwise refetch the pre-install list and stop looking.
	boostPluginPolling();
	return Promise.all([
		qc.invalidateQueries({ queryKey: getGetPluginCatalogQueryKey() }),
		qc.invalidateQueries({ queryKey: getListInstalledPluginsQueryKey() }),
		qc.invalidateQueries({ queryKey: getListPluginSourcesQueryKey() }),
		qc.invalidateQueries({ queryKey: getGetPluginRuntimeQueryKey() }),
		qc.invalidateQueries({ queryKey: getListPluginsQueryKey() }),
	]).then(() => undefined);
}

/** What's installed right now, with each plugin's permanent provenance tier. */
export const useInstalledPlugins = () =>
	useListInstalledPlugins({ query: { refetchInterval: 30_000 } });

/**
 * A single install/uninstall job, polled once a second while it runs and left alone once it
 * settles. Pass `null` to park the query (no job in flight).
 *
 * The interval keys off "not finished yet" rather than off `state === "running"`. `data` is
 * undefined in two live cases — the first poll has not landed, and the first poll FAILED — and
 * treating those as "stop polling" wedged the card: an install whose very first poll lost the race
 * with a busy host never polled again and the operator saw nothing at all, while an install that
 * restarts the runner (every successful one does) could drop a poll mid-flight.
 *
 * The failure count bounds it: jobs live in host memory, so a host restart makes the id 404 for
 * good, and something has to stop asking.
 */
const JOB_POLL_MS = 1_000;
const JOB_MAX_FAILURES = 15;

/**
 * The host's recent jobs, used to RE-ATTACH after a reload.
 *
 * The in-flight job id lived only in component state, so reloading the page (or opening the console
 * on another device) lost all trace of a running install while the Install buttons stayed armed —
 * and the host takes one job at a time, so the next click just bounced off a 409. The host keeps
 * the list; ask it rather than remembering.
 */
export const useStoreJobs = () =>
	// Only needed to find an orphaned job on mount; the job query itself does the live polling.
	useListPluginJobs({ query: { staleTime: 5_000 } });

/** The newest job that is still running, if any — what a fresh page should re-attach to. */
export function runningJob(jobs: Job[] | undefined): Job | undefined {
	if (!jobs) return undefined;
	// The list is oldest-first, so scan from the end for the most recent live one.
	for (let i = jobs.length - 1; i >= 0; i--) {
		const j = jobs[i];
		if (j?.state === "running") return j;
	}
	return undefined;
}

export function useStoreJob(id: string | null) {
	return useGetPluginJob(id ?? "", {
		query: {
			enabled: id !== null,
			refetchInterval: (q) => {
				const state = q.state.data?.state;
				if (state === "done" || state === "failed") return false;
				if (q.state.fetchFailureCount > JOB_MAX_FAILURES) return false;
				return JOB_POLL_MS;
			},
			// A job that vanished with its host is gone for good; a transient blip is not. Retry a
			// few times per poll so a runner restart doesn't surface as an error card.
			retry: 3,
		},
	});
}

/** Re-fetch every source's index; answers with the freshly merged catalog. */
export function useRefreshCatalog() {
	const qc = useQueryClient();
	return useRefreshPluginCatalog({
		mutation: {
			onSuccess: (catalog) => {
				qc.setQueryData(getGetPluginCatalogQueryKey(), catalog);
				qc.setQueryData(getListPluginSourcesQueryKey(), catalog.sources);
			},
		},
	});
}

/** Start an install. Answers 202 with the job to poll; 409 means another op is already running. */
export function useInstallPlugin() {
	return useMutation({
		mutationFn: (body: InstallBody) =>
			apiFetch<JobRef>(`${BASE}/install`, json("POST", body)),
	});
}

/** Add or update an operator source (the built-in one is not editable). */
export function useSetSource() {
	const qc = useQueryClient();
	return useMutation({
		mutationFn: ({ name, ...body }: SourceBody & { name: string }) =>
			apiFetch<void>(
				`${BASE}/sources/${encodeURIComponent(name)}`,
				json("PUT", body),
			),
		onSuccess: () => invalidateSources(qc),
	});
}

export function useDeleteSource() {
	const qc = useQueryClient();
	return useDeletePluginSource({
		mutation: { onSuccess: () => invalidateSources(qc) },
	});
}

const invalidateSources = (qc: QueryClient) => {
	qc.invalidateQueries({ queryKey: getListPluginSourcesQueryKey() });
	qc.invalidateQueries({ queryKey: getGetPluginCatalogQueryKey() });
};

/** Enable or disable the plugin/script runner service. */
export function useSetRuntime() {
	const qc = useQueryClient();
	return useSetPluginRuntime({
		mutation: {
			onSuccess: (status) => {
				qc.setQueryData(getGetPluginRuntimeQueryKey(), status);
				qc.invalidateQueries({ queryKey: getListPluginsQueryKey() });
			},
		},
	});
}
