import { useQueryClient } from "@tanstack/react-query";
import {
	getRouteApi,
	Link,
	useBlocker,
	useNavigate,
} from "@tanstack/react-router";
import Section from "@unom/ui/section";
import { toast } from "@unom/ui/toast";
import { type FC, useRef, useState } from "react";
import {
	getGetCustomGameQueryKey,
	useCreateCustomGame,
	useDeleteCustomGame,
	useGetCustomGame,
	useGetLibraryPage,
	useSetLibraryEntryHidden,
	useUpdateCustomGame,
} from "@/api/gen/library/library";
import type { OperatorGameEntry } from "@/api/gen/model/operatorGameEntry";
import { useDialogs } from "@/components/dialogs";
import { passwordFailure } from "@/components/password-confirm";
import { QueryState } from "@/components/query-state";
import { SeatScope } from "@/components/seat-scope";
import { Card, CardContent } from "@/components/ui/card";
import { carriesCommandExecution } from "@/lib/command-execution";
import { apiErrorMessage } from "@/lib/errors";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";
import {
	customId,
	isOperatorOwned,
	refreshLibrary,
	storeLabel,
} from "../helpers";
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
import { EntryView } from "./view";

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
	const creating = gameId === "new";
	// This one title, not the library it belongs to.
	const library = useGetLibraryPage(
		{ id: gameId, limit: 1 },
		{ query: { enabled: !creating } },
	);
	if (creating)
		return <EntryEditor key="new" entry={null} initial={emptyForm} />;
	const entry = library.data?.items[0];
	return (
		<QueryState
			isLoading={library.isLoading}
			error={library.error}
			refetch={library.refetch}
		>
			{entry ? (
				isOperatorOwned(entry) ? (
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

const EntryEditor: FC<{
	entry: OperatorGameEntry | null;
	initial: FormState;
	/** The operator owns the entry. Implied while creating. */
	owned?: boolean;
}> = ({ entry, initial, owned }) => {
	const qc = useQueryClient();
	const navigate = useNavigate();
	const { tab } = route.useSearch();
	const { confirm } = useDialogs();
	const nameOf = useSourceNames();
	const [draft, setDraft] = useState(initial);
	const [baseline, setBaseline] = useState(initial);
	const [password, setPassword] = useState("");
	const pluginTabs = usePluginTabs(entry?.id ?? null);
	const create = useCreateCustomGame();
	const update = useUpdateCustomGame();
	const remove = useDeleteCustomGame();
	const setHidden = useSetLibraryEntryHidden();
	// Set before a navigation this page starts itself, so the blocker lets it through.
	const leaving = useRef(false);

	const readOnly = entry !== null && !owned;
	const dirty = !readOnly && JSON.stringify(draft) !== JSON.stringify(baseline);
	const input = toInput(draft);
	const gated = !readOnly && carriesCommandExecution(input);

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

	const save = async () => {
		if (!input.title) return;
		const body = withPassword(input, password);
		try {
			if (entry === null) {
				const row = await create.mutateAsync({ data: body });
				leaving.current = true;
				await refresh();
				navigate({
					to: "/library/$gameId",
					params: { gameId: `custom:${row.id}` },
					replace: true,
				});
				return;
			}
			await update.mutateAsync({ id: customId(entry), data: body });
		} catch {
			return; // shown from the mutation's error below
		}
		setBaseline(draft);
		setPassword("");
		toast.success(m.library_entry_saved());
		refresh();
		qc.invalidateQueries({
			queryKey: getGetCustomGameQueryKey(customId(entry)),
		});
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
			toast.error(apiErrorMessage(e) ?? m.library_delete_failed());
			return;
		}
		leaving.current = true;
		await refresh();
		navigate({ to: "/library" });
	};

	const onToggleHidden = async () => {
		if (!entry) return;
		try {
			await setHidden.mutateAsync({
				id: entry.id,
				data: { hidden: entry.hidden !== true },
			});
		} catch (e) {
			toast.error(apiErrorMessage(e) ?? m.library_hide_failed());
			return;
		}
		refresh();
	};

	const source = entry?.provider ?? entry?.store;
	const saveError = create.error ?? update.error;
	const failure = passwordFailure(saveError);
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
				storeName: entry ? storeLabel(entry.store, nameOf) : null,
				managedBy:
					readOnly && source ? (nameOf(source) ?? storeLabel(source)) : null,
				dirty,
				saving: create.isPending || update.isPending,
				gated,
				password,
				onPassword: setPassword,
				failure,
				onSave: readOnly ? undefined : save,
				onDelete: entry && owned ? onDelete : undefined,
				deleting: remove.isPending,
				onToggleHidden: entry ? onToggleHidden : undefined,
				hiding: setHidden.isPending,
				error: failure ? null : apiErrorMessage(saveError),
			}}
		/>
	);
};
