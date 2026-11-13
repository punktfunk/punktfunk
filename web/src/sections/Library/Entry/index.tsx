import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
	getRouteApi,
	Link,
	useBlocker,
	useNavigate,
} from "@tanstack/react-router";
import Section from "@unom/ui/section";
import { toast } from "@unom/ui/toast";
import { type FC, useEffect, useRef, useState } from "react";
import { ApiError, inSeat } from "@/api/fetcher";
import {
	createCustomGame,
	deleteCustomGame,
	getCustomGame,
	getGetCustomGameQueryKey,
	updateCustomGame,
	useCreateCustomGame,
	useDeleteCustomGame,
	useGetCustomGame,
	useGetLibraryPage,
	useSetLibraryEntryHidden,
	useUpdateCustomGame,
} from "@/api/gen/library/library";
import type { CustomEntry } from "@/api/gen/model/customEntry";
import type { CustomInput } from "@/api/gen/model/customInput";
import type { OperatorGameEntry } from "@/api/gen/model/operatorGameEntry";
import {
	type ProfileView,
	seatClient,
	useAs,
	useProfileView,
} from "@/api/seat";
import { useDialogs } from "@/components/dialogs";
import { passwordFailure } from "@/components/password-confirm";
import { QueryState } from "@/components/query-state";
import { SeatScope } from "@/components/seat-scope";
import { Card, CardContent } from "@/components/ui/card";
import { carriesCommandExecution } from "@/lib/command-execution";
import { apiErrorMessage } from "@/lib/errors";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";
import { isAbort, liveDeps, onDesktop, useDesktop } from "../desktop";
import {
	customId,
	dropTitle,
	isOperatorOwned,
	isOwnId,
	ownEntry,
	refreshLibrary,
	storeLabel,
} from "../helpers";
import { SharedNote } from "../SharedNote";
import { useSourceNames } from "../Sources";
import {
	emptyForm,
	type FormState,
	formFrom,
	formFromStored,
	toInput,
	withPassword,
} from "./model";
import { usePluginTabs } from "./PluginTab";
import { EntryView, OwnEntryWait } from "./view";

const route = getRouteApi("/library_/$gameId");

/** `/library/$gameId`: one library entry, or `new` to create a custom one. */
export const SectionLibraryEntry: FC = () => (
	<SeatScope page="library">
		<LibraryEntry />
	</SeatScope>
);

const LibraryEntry: FC = () => {
	useLocale();
	const { gameId } = route.useParams();
	const view = useProfileView();
	const as = useAs();
	const creating = gameId === "new";
	// This one title, not the library it belongs to; in a profile's view, as it has it.
	const library = useGetLibraryPage(
		{ id: gameId, limit: 1, ...as },
		{ query: { enabled: !creating } },
	);
	// A new title is the owner's library's, but for a game only the profile in view has: a
	// profile's view adds that one, or says where to add.
	if (creating && view && !view.ownGames) return <SharedOnly />;
	if (creating)
		return (
			<EntryEditor key="new" entry={null} initial={emptyForm} own={!!view} />
		);
	const entry = library.data?.items[0];
	return (
		<QueryState
			isLoading={library.isLoading}
			error={library.error}
			refetch={library.refetch}
		>
			{view?.ownGames && isOwnId(gameId) ? (
				<OwnEntry key={gameId} id={gameId} listed={entry} view={view} />
			) : entry ? (
				isOperatorOwned(entry) && !view ? (
					<OwnedEntry key={entry.id} entry={entry} />
				) : (
					<EntryEditor key={entry.id} entry={entry} initial={formFrom(entry)} />
				)
			) : (
				<Missing />
			)}
		</QueryState>
	);
};

/** A custom entry edits from its stored row: the catalog's art is proxied, the row's is not. */
const OwnedEntry: FC<{ entry: OperatorGameEntry }> = ({ entry }) => {
	const row = useGetCustomGame(customId(entry));
	return (
		<QueryState
			isLoading={row.isLoading}
			error={row.error}
			refetch={row.refetch}
		>
			{row.data && (
				<EntryEditor entry={entry} initial={formFromStored(row.data)} owned />
			)}
		</QueryState>
	);
};

/**
 * A game only the profile in view has. The box lists its name and pictures; its desktop keeps the
 * rest, and starts first when it is stopped. Meanwhile the page shows what the box lists. Only
 * opening the page or Retry starts the desktop: a refresh keeps the row it holds.
 */
const OwnEntry: FC<{
	id: string;
	listed: OperatorGameEntry | undefined;
	view: ProfileView;
}> = ({ id, listed, view }) => {
	const desktop = seatClient(view.id, useQueryClient());
	const [waiting, setWaiting] = useState(false);
	const key = getGetCustomGameQueryKey(id);
	const row = useQuery(
		{
			queryKey: key,
			queryFn: ({ signal }) =>
				desktop.getQueryData<CustomEntry>(key) ??
				onDesktop({
					name: view.name,
					call: () => inSeat(view.id, () => getCustomGame(id, { signal })),
					signal,
					onWait: setWaiting,
					deps: liveDeps(view.id),
				}),
			retry: false,
			staleTime: Number.POSITIVE_INFINITY,
			refetchOnWindowFocus: false,
		},
		desktop,
	);
	if (row.data)
		return (
			<EntryEditor
				entry={ownEntry(listed, row.data, view.id)}
				initial={formFromStored(row.data)}
				owned
				own
			/>
		);
	if (row.error instanceof ApiError && row.error.status === 404)
		return <Missing />;
	return (
		<OwnEntryWait
			listed={listed}
			who={view.name}
			waiting={waiting}
			error={row.error}
			onRetry={() => void row.refetch()}
		/>
	);
};

const SharedOnly: FC = () => (
	<Section maxWidth={false}>
		<SharedNote />
	</Section>
);

const Missing: FC = () => (
	<Section maxWidth={false}>
		<Card>
			<CardContent className="space-y-3 pt-card">
				<p>{m.library_entry_missing()}</p>
				<Link
					to="/library"
					className="text-sm text-muted-foreground underline hover:text-foreground"
				>
					{m.library_title()}
				</Link>
			</CardContent>
		</Card>
	</Section>
);

/** The mutation options that send a call through `run`, the profile's desktop's; none without. */
const through = <V, T>(
	run: (<R>(call: () => Promise<R>) => Promise<R>) | null,
	call: (v: V) => Promise<T>,
) => (run ? { mutation: { mutationFn: (v: V) => run(() => call(v)) } } : {});

const EntryEditor: FC<{
	entry: OperatorGameEntry | null;
	initial: FormState;
	/** The operator owns the entry. Implied while creating. */
	owned?: boolean;
	/** A game only the profile in view has, or one added for it: its desktop stores it. */
	own?: boolean;
}> = ({ entry, initial, owned, own = false }) => {
	const qc = useQueryClient();
	const navigate = useNavigate();
	const { tab } = route.useSearch();
	const { confirm } = useDialogs();
	const nameOf = useSourceNames();
	const view = useProfileView();
	const as = useAs();
	const [draft, setDraft] = useState(initial);
	const [baseline, setBaseline] = useState(initial);
	const [password, setPassword] = useState("");
	// A plugin's own tab is the owner's library's.
	const pluginTabs = usePluginTabs(view ? null : (entry?.id ?? null));
	const mine = own && view ? view : null;
	const desktop = useDesktop(mine);
	const run = mine ? desktop.run : null;
	const create = useCreateCustomGame(
		through(run, (v: { data: CustomInput }) => createCustomGame(v.data)),
	);
	const update = useUpdateCustomGame(
		through(run, (v: { id: string; data: CustomInput }) =>
			updateCustomGame(v.id, v.data),
		),
	);
	const remove = useDeleteCustomGame(
		through(run, (v: { id: string }) => deleteCustomGame(v.id)),
	);
	// Leaving drops a save that still waits for the desktop: the person chose to discard it.
	useEffect(() => desktop.cancel, [desktop.cancel]);
	const setHidden = useSetLibraryEntryHidden();
	// Set before a navigation this page starts itself, so the blocker lets it through.
	const leaving = useRef(false);

	const readOnly = entry !== null && !owned;
	const dirty = !readOnly && JSON.stringify(draft) !== JSON.stringify(baseline);
	const input = toInput(draft);
	// A person's own console has no password to ask: their seat host takes the command as theirs.
	const gated = !readOnly && carriesCommandExecution(input) && !view?.person;

	useBlocker({
		shouldBlockFn: async ({ current, next }) => {
			if (!dirty || leaving.current) return false;
			if (current.pathname === next.pathname) return false; // a tab switch
			return !(await confirm({
				title: m.library_entry_leave_title(),
				description: m.library_entry_leave_body(),
				confirmLabel: m.library_entry_leave_confirm(),
				destructive: true,
			}));
		},
		enableBeforeUnload: () => dirty && !leaving.current,
	});

	const refresh = () => refreshLibrary(qc);
	// A game only the profile has is stored on its desktop, whose answer is the stored row.
	const keep = (row: CustomEntry) => {
		if (mine)
			seatClient(mine.id, qc).setQueryData(
				getGetCustomGameQueryKey(row.id),
				row,
			);
	};

	const save = async () => {
		if (!input.title) return;
		const body = view?.person ? input : withPassword(input, password);
		try {
			if (entry === null) {
				const row = await create.mutateAsync({ data: body });
				leaving.current = true;
				keep(row);
				await refresh();
				navigate({
					to: "/library/$gameId",
					params: { gameId: mine ? row.id : `custom:${row.id}` },
					replace: true,
				});
				return;
			}
			const row = await update.mutateAsync({
				id: customId(entry),
				data: body,
			});
			if (mine) keep(row);
			else
				qc.invalidateQueries({
					queryKey: getGetCustomGameQueryKey(customId(entry)),
				});
		} catch {
			return; // shown from the mutation's error below
		}
		setBaseline(draft);
		setPassword("");
		toast.success(m.library_entry_saved());
		refresh();
	};

	const onDelete = async () => {
		if (!entry) return;
		const ok = await confirm({
			title: m.library_delete_confirm(),
			description: m.library_delete_body(),
			confirmLabel: m.library_delete(),
			destructive: true,
		});
		if (!ok) return;
		try {
			await remove.mutateAsync({ id: customId(entry) });
		} catch (e) {
			if (!isAbort(e))
				toast.error(apiErrorMessage(e) ?? m.library_delete_failed());
			return;
		}
		leaving.current = true;
		// The box lists a game only the profile had a moment longer: it leaves the pages here.
		if (mine) dropTitle(qc, entry.id);
		else await refresh();
		navigate({ to: "/library" });
	};

	const onToggleHidden = async () => {
		if (!entry) return;
		try {
			await setHidden.mutateAsync({
				id: entry.id,
				data: { hidden: entry.hidden !== true },
				params: as,
			});
		} catch (e) {
			toast.error(apiErrorMessage(e) ?? m.library_hide_failed());
			return;
		}
		refresh();
	};

	const source = entry?.provider ?? entry?.store;
	const tried = create.error ?? update.error;
	// A cancelled wait for the desktop is no failure.
	const saveError = isAbort(tried) ? null : tried;
	const failure = passwordFailure(saveError);
	const name = mine?.name ?? "";
	return (
		<EntryView
			entry={entry}
			draft={draft}
			baseline={baseline}
			readOnly={readOnly}
			set={(key, value) => setDraft((d) => ({ ...d, [key]: value }))}
			tab={tab ?? "information"}
			pluginTabs={pluginTabs}
			onTab={(next) =>
				navigate({
					to: "/library/$gameId",
					params: { gameId: entry?.id ?? "new" },
					search: { tab: next },
					replace: true,
				})
			}
			header={{
				// A game only the profile has carries its mark in place of a store.
				storeName: entry && !own ? storeLabel(entry.store, nameOf) : null,
				// A profile's view says under the header that the title is the owner's library's.
				managedBy:
					readOnly && source && !view
						? (nameOf(source) ?? storeLabel(source))
						: null,
				...(view ? { who: view.name } : {}),
				own,
				...(desktop.waiting
					? {
							waiting: {
								line: remove.isPending
									? m.profile_view_desktop_wait_delete({ name })
									: m.profile_view_desktop_wait_save({ name }),
								onCancel: desktop.cancel,
							},
						}
					: {}),
				dirty,
				saving: create.isPending || update.isPending,
				gated,
				password,
				onPassword: setPassword,
				failure,
				onSave: readOnly ? undefined : save,
				onDelete: entry && owned ? onDelete : undefined,
				deleting: remove.isPending,
				// Hiding a game only the profile has makes no sense: delete is the way.
				onToggleHidden: entry && !own ? onToggleHidden : undefined,
				hiding: setHidden.isPending,
				error: failure ? null : apiErrorMessage(saveError),
			}}
		/>
	);
};
