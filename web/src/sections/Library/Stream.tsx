// Game overrides in the console: the Stream controls of a game or a source, and the Shared
// overrides dialog. Each control writes to the host when it changes and then shows what the host
// stored, apart from the entry page's draft and Save.
import { type QueryClient, useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { Pencil, Plus, Trash2 } from "lucide-react";
import { type FC, type ReactNode, useState } from "react";
import {
	getGetLibraryOverridesQueryKey,
	useCreateOverrideSet,
	useDeleteOverrideSet,
	useGetLibraryOverrides,
	useSetGameOverrides,
	useSetSourceOverrides,
	useUpdateOverrideSet,
} from "@/api/gen/library/library";
import type { OverrideEntry } from "@/api/gen/model/overrideEntry";
import { useDialogs } from "@/components/dialogs";
import { QueryState } from "@/components/query-state";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
	Dialog,
	DialogContent,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { Label } from "@/components/ui/label";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { apiErrorMessage } from "@/lib/errors";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";
import { refreshLibrary } from "./helpers";
import {
	applyPatch,
	type Fields,
	fieldsOf,
	linesLabel,
	type Patch,
	padLabel,
	padNames,
	REFRESH_RATES,
	RESOLUTIONS,
	setList,
	storeOf,
	usageOf,
} from "./overrides";

/** `Select` values beside the set ids, which are 12 hex and never these. */
const DEVICE = "device";
const CUSTOM = "custom";
const NEW = "new";

/** The document and every library view ask again: a badge follows the entry it summarizes. */
export const refreshOverrides = (qc: QueryClient) =>
	Promise.all([
		qc.invalidateQueries({ queryKey: getGetLibraryOverridesQueryKey() }),
		refreshLibrary(qc),
	]);

/** One write at a time, then the stored state: a refused write toasts and the control reverts. */
function useStreamWrite() {
	const qc = useQueryClient();
	const [busy, setBusy] = useState(false);
	const run = async <T,>(write: () => Promise<T>): Promise<T | undefined> => {
		setBusy(true);
		try {
			return await write();
		} catch (e) {
			toast.error(apiErrorMessage(e) ?? m.library_stream_failed());
			return undefined;
		} finally {
			await refreshOverrides(qc);
			setBusy(false);
		}
	};
	return { busy, run };
}

/** Asks for a shared override's name; null when the operator backs out or leaves it blank. */
function useAskName() {
	const { promptText } = useDialogs();
	return async (title: string, confirmLabel: string, defaultValue?: string) =>
		(
			await promptText({
				title,
				label: m.library_stream_name(),
				confirmLabel,
				...(defaultValue ? { defaultValue } : {}),
			})
		)?.trim() || null;
}

const Choice: FC<{
	id: string;
	label: string;
	value: string;
	options: [string, string][];
	onPick: (value: string) => void;
	disabled: boolean;
	/** Where the value comes from, when not from here; the value reads muted. */
	note?: string;
	children?: ReactNode;
}> = ({ id, label, value, options, onPick, disabled, note, children }) => (
	<div className="space-y-2">
		<Label htmlFor={id}>{label}</Label>
		<Select value={value} onValueChange={onPick} disabled={disabled}>
			<SelectTrigger
				id={id}
				size="sm"
				className={cn(note && "text-muted-foreground")}
			>
				<SelectValue />
			</SelectTrigger>
			<SelectContent>
				{options.map(([v, text]) => (
					<SelectItem key={v} value={v}>
						{text}
					</SelectItem>
				))}
			</SelectContent>
		</Select>
		{note && <p className="text-xs text-muted-foreground">{note}</p>}
		{children}
	</div>
);

/** "Device decides", then `list` and a stored value it lacks, labelled. */
function choices<T extends string | number>(
	list: readonly T[],
	current: T | null | undefined,
	label: (v: T) => string,
): [string, string][] {
	const all =
		current == null || list.includes(current) ? list : [...list, current];
	return [
		[DEVICE, m.library_stream_device()],
		...all.map((v): [string, string] => [String(v), label(v)]),
	];
}

const picked = (v: string) => (v === DEVICE ? null : v);

/** Resolution with its screen option, refresh rate, HDR and controller. */
export const StreamFields: FC<{
	id: string;
	value: Fields;
	onSet: (patch: Patch) => void;
	disabled: boolean;
	/** The source these values come from, for a game that has none of its own. */
	from?: string | undefined;
}> = ({ id, value, onSet, disabled, from }) => {
	const note = (set: boolean) =>
		from && set ? m.library_stream_from({ source: from }) : undefined;
	const hdr = value.hdr == null ? DEVICE : value.hdr ? "on" : "off";
	return (
		<div className="grid gap-4 @md:grid-cols-2">
			<Choice
				id={`${id}-lines`}
				label={m.library_stream_resolution()}
				value={value.lines != null ? String(value.lines) : DEVICE}
				options={choices(RESOLUTIONS, value.lines, linesLabel)}
				onPick={(v) => {
					const lines = picked(v);
					onSet(
						lines == null
							? { lines: null, beyond_screen: null }
							: { lines: Number(lines) },
					);
				}}
				disabled={disabled}
				note={note(value.lines != null)}
			>
				{value.lines != null && (
					<div className="flex items-center gap-2">
						<Checkbox
							id={`${id}-beyond`}
							checked={value.beyond_screen === true}
							disabled={disabled}
							onCheckedChange={(next) =>
								onSet({ beyond_screen: next === true ? true : null })
							}
						/>
						<Label htmlFor={`${id}-beyond`}>{m.library_stream_beyond()}</Label>
					</div>
				)}
			</Choice>
			<Choice
				id={`${id}-hz`}
				label={m.library_stream_refresh()}
				value={value.hz != null ? String(value.hz) : DEVICE}
				options={choices(REFRESH_RATES, value.hz, (hz) =>
					m.library_stream_hz({ hz }),
				)}
				onPick={(v) => {
					const hz = picked(v);
					onSet({ hz: hz == null ? null : Number(hz) });
				}}
				disabled={disabled}
				note={note(value.hz != null)}
			/>
			<Choice
				id={`${id}-hdr`}
				label={m.library_stream_hdr()}
				value={hdr}
				options={[
					[DEVICE, m.library_stream_device()],
					["on", m.common_on()],
					["off", m.common_off()],
				]}
				onPick={(v) => onSet({ hdr: v === DEVICE ? null : v === "on" })}
				disabled={disabled}
				note={note(value.hdr != null)}
			/>
			<Choice
				id={`${id}-pad`}
				label={m.library_stream_controller()}
				value={value.gamepad ?? DEVICE}
				options={choices(padNames(), value.gamepad, padLabel)}
				onPick={(v) => onSet({ gamepad: picked(v) })}
				disabled={disabled}
				note={note(value.gamepad != null)}
			/>
		</div>
	);
};

/**
 * A game's or a source's stream settings: *Use* (none, its own values, or a shared override),
 * then the values. A shared override's values read here and change in its dialog. A game with no
 * entry shows its source's, muted, and editing one makes them the game's own.
 */
export const StreamControls: FC<{
	kind: "game" | "source";
	/** The library id, or the source id. */
	id: string;
	/** A game's source by name, for "from Lutris". */
	from?: string;
}> = ({ kind, id, from }) => {
	const doc = useGetLibraryOverrides();
	const setGame = useSetGameOverrides();
	const setSource = useSetSourceOverrides();
	const create = useCreateOverrideSet();
	const { busy, run } = useStreamWrite();
	const askName = useAskName();
	// Custom with nothing set yet stores nothing; the pick holds until a value does.
	const [custom, setCustom] = useState(false);
	const [editing, setEditing] = useState<string | null>(null);

	const d = doc.data;
	const own = kind === "game" ? d?.games?.[id] : d?.sources?.[id];
	const inherited =
		!own && kind === "game" ? d?.sources?.[storeOf(id)] : undefined;
	const entry = own ?? inherited;
	// A set id the document no longer holds resolves to nothing, as on the host.
	const setId = entry?.set && d?.sets?.[entry.set] ? entry.set : undefined;
	const values = Object.keys(fieldsOf(entry)).length > 0;
	const shown = setId ? fieldsOf(d?.sets?.[setId]) : fieldsOf(entry);
	const use = setId ?? (values || custom ? CUSTOM : DEVICE);
	const via = inherited && (setId || values) ? from : undefined;

	const put = (data: OverrideEntry) =>
		kind === "game"
			? setGame.mutateAsync({ id, data })
			: setSource.mutateAsync({ source: id, data });

	const onUse = async (v: string) => {
		if (v === NEW) {
			const name = await askName(
				m.library_stream_new_title(),
				m.common_create(),
			);
			if (!name) return;
			await run(async () => {
				const made = await create.mutateAsync({ data: { ...shown, name } });
				await put({ set: made.id });
			});
			return;
		}
		setCustom(v === CUSTOM);
		if (v === CUSTOM) {
			// The shared override's or the source's values become this entry's own, to edit.
			if ((setId || inherited) && Object.keys(shown).length > 0)
				await run(() => put(shown));
			return;
		}
		await run(() => put(v === DEVICE ? {} : { set: v }));
	};

	const htmlId = `stream-${kind}-${id}`;
	return (
		<QueryState
			isLoading={doc.isLoading}
			error={doc.error}
			refetch={doc.refetch}
		>
			<div className="space-y-4">
				<div className="max-w-sm">
					<Choice
						id={`${htmlId}-use`}
						label={m.library_stream_use()}
						value={use}
						options={[
							[DEVICE, m.library_stream_device()],
							[CUSTOM, m.library_stream_custom()],
							...setList(d).map((s): [string, string] => [
								s.id,
								s.name ?? s.id,
							]),
							[NEW, m.library_stream_new()],
						]}
						onPick={(v) => void onUse(v)}
						disabled={busy}
						note={via && m.library_stream_from({ source: via })}
					>
						{setId && (
							<Button
								variant="link"
								size="sm"
								className="h-auto p-0"
								onClick={() => setEditing(setId)}
							>
								{m.library_stream_edit_shared()}
							</Button>
						)}
					</Choice>
				</div>
				<StreamFields
					id={htmlId}
					value={shown}
					onSet={(patch) => void run(() => put(applyPatch(shown, patch)))}
					disabled={busy || setId !== undefined}
					from={via}
				/>
			</div>
			{editing && (
				<SharedOverridesDialog
					only={editing}
					onClose={() => setEditing(null)}
				/>
			)}
		</QueryState>
	);
};

/** "12 games · 1 source", or that nothing uses it. */
function usageLine({ games, sources }: { games: number; sources: number }) {
	const parts = [
		games > 0 && m.library_stream_used_games({ count: games }),
		sources > 0 && m.library_stream_used_sources({ count: sources }),
	].filter(Boolean);
	return parts.length ? parts.join(" · ") : m.library_stream_unused();
}

/**
 * The shared overrides: what uses each, its values, rename and delete, and a new one. `only`
 * narrows it to one, opened from a game that uses it.
 */
export const SharedOverridesDialog: FC<{
	only?: string;
	onClose: () => void;
}> = ({ only, onClose }) => {
	const doc = useGetLibraryOverrides();
	const create = useCreateOverrideSet();
	const update = useUpdateOverrideSet();
	const remove = useDeleteOverrideSet();
	const { busy, run } = useStreamWrite();
	const { confirm } = useDialogs();
	const askName = useAskName();
	const sets = setList(doc.data).filter((s) => !only || s.id === only);

	const onNew = async () => {
		const name = await askName(m.library_stream_new_title(), m.common_create());
		if (name) await run(() => create.mutateAsync({ data: { name } }));
	};
	const onRename = async (id: string, current: string) => {
		const name = await askName(
			m.library_stream_rename_title(),
			m.common_rename(),
			current,
		);
		if (name && name !== current)
			await run(() => update.mutateAsync({ id, data: { name } }));
	};
	const onDelete = async (id: string, name: string) => {
		const { games, sources } = usageOf(doc.data, id);
		const body = [
			games > 0 && m.library_stream_delete_body({ count: games }),
			sources > 0 && m.library_stream_delete_sources_body({ count: sources }),
		]
			.filter(Boolean)
			.join(" ");
		const ok = await confirm({
			title: m.library_stream_delete_title({ name }),
			...(body ? { description: body } : {}),
			confirmLabel: m.common_delete(),
			destructive: true,
		});
		if (!ok) return;
		const gone = await run(() => remove.mutateAsync({ id }));
		if (gone && only) onClose();
	};

	return (
		<Dialog open onOpenChange={(open) => !open && onClose()}>
			<DialogContent className="max-h-[85vh] overflow-y-auto sm:max-w-xl">
				<DialogHeader>
					<DialogTitle>{m.library_stream_shared()}</DialogTitle>
				</DialogHeader>
				<QueryState
					isLoading={doc.isLoading}
					error={doc.error}
					refetch={doc.refetch}
				>
					{sets.length === 0 ? (
						<p className="text-sm text-muted-foreground">
							{m.library_stream_shared_empty()}
						</p>
					) : (
						<ul className="@container divide-y">
							{sets.map((s) => {
								const name = s.name ?? s.id;
								return (
									<li key={s.id} className="space-y-3 py-4 first:pt-0">
										<div className="flex items-center gap-2">
											<div className="min-w-0 flex-1">
												<p className="truncate font-medium">{name}</p>
												<p className="text-xs text-muted-foreground">
													{usageLine(usageOf(doc.data, s.id))}
												</p>
											</div>
											<Button
												variant="ghost"
												size="icon"
												className="size-8"
												aria-label={m.common_rename()}
												title={m.common_rename()}
												disabled={busy}
												onClick={() => void onRename(s.id, name)}
											>
												<Pencil className="size-4" />
											</Button>
											<Button
												variant="ghost"
												size="icon"
												className="size-8"
												aria-label={m.common_delete()}
												title={m.common_delete()}
												disabled={busy}
												onClick={() => void onDelete(s.id, name)}
											>
												<Trash2 className="size-4 text-destructive" />
											</Button>
										</div>
										<StreamFields
											id={`stream-set-${s.id}`}
											value={fieldsOf(s)}
											disabled={busy}
											onSet={(patch) =>
												void run(() =>
													update.mutateAsync({ id: s.id, data: patch }),
												)
											}
										/>
									</li>
								);
							})}
						</ul>
					)}
					{!only && (
						<Button
							variant="outline"
							className="mt-4"
							disabled={busy}
							onClick={() => void onNew()}
						>
							<Plus className="size-4" />
							{m.library_stream_new()}
						</Button>
					)}
				</QueryState>
			</DialogContent>
		</Dialog>
	);
};
