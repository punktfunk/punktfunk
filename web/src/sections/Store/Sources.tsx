import { toast } from "@unom/ui/toast";
import {
	AlertTriangle,
	Globe,
	Lock,
	Plus,
	RefreshCw,
	ShieldCheck,
	ShieldOff,
	Trash2,
} from "lucide-react";
import { motion } from "motion/react";
import { type FC, type FormEvent, useState } from "react";
import { ApiError } from "@/api/fetcher";
import type { SourceView } from "@/api/gen/model";
import { useListPluginSources } from "@/api/gen/store/store";
import {
	type SourceBody,
	useDeleteSource,
	useRefreshCatalog,
	useSetSource,
} from "@/api/store";
import { useDialogs } from "@/components/dialogs";
import { PasswordConfirmDialog } from "@/components/password-confirm";
import { QueryState } from "@/components/query-state";
import { ROW, ROW_GAP, Stagger } from "@/components/stagger";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { fmtDateTimeSecs } from "@/lib/format";
import { m } from "@/paraglide/messages";

/** A source the operator has filled in but not yet agreed to trust. The console password is NOT
 * part of the draft — it is collected by the trust dialog, at the moment the decision is made. */
type SourceDraft = Omit<SourceBody, "password"> & { name: string };

/** Unix seconds → a locale date-time, or "never" for a source that has never fetched.
 * Locale-aware via lib/format.ts — `toLocaleString` follows the browser, not the console. */
const fmtFetched = (secs?: number | null): string =>
	secs ? fmtDateTimeSecs(secs) : m.store_source_never();

/**
 * Container: the catalog sources. Owns the source listing, the refresh-all action, and add/remove.
 * Adding is a two-step: the form collects the source, and a one-time trust dialog states plainly
 * what trusting a third-party catalog means before anything is written to the host.
 */
export const SourcesTab: FC = () => {
	const { confirm } = useDialogs();
	const sources = useListPluginSources();
	const refresh = useRefreshCatalog();
	const save = useSetSource();
	const remove = useDeleteSource();
	// The draft waiting on the trust dialog, and a key that re-mounts (and so clears) the form.
	const [draft, setDraft] = useState<SourceDraft | null>(null);
	const [formKey, setFormKey] = useState(0);

	const onRefresh = () =>
		refresh.mutate(undefined, {
			onError: () => toast.error(m.store_refresh_failed()),
		});

	const onConfirmAdd = async (draft: SourceDraft, password: string) => {
		await save.mutateAsync({ ...draft, password });
		setFormKey((k) => k + 1);
	};

	const onRemove = async (source: SourceView) => {
		const ok = await confirm({
			title: m.store_source_remove_confirm({ name: source.name }),
			description: m.store_source_remove_body(),
			confirmLabel: m.store_source_remove(),
			destructive: true,
		});
		if (!ok) return;
		try {
			await remove.mutateAsync({ name: source.name });
		} catch (e) {
			// 403 is the host refusing to drop its built-in catalog — say exactly that.
			toast.error(
				e instanceof ApiError && e.status === 403
					? m.store_source_builtin_locked()
					: m.store_source_remove_failed(),
			);
		}
	};

	return (
		<div className="flex flex-col gap-card">
			<SourceList
				sources={sources}
				busyName={remove.isPending ? (remove.variables?.name ?? null) : null}
				isRefreshing={refresh.isPending}
				onRefresh={onRefresh}
				onRemove={onRemove}
			/>

			<AddSourceForm
				key={formKey}
				onSubmit={setDraft}
				isSaving={save.isPending}
			/>

			<TrustSourceDialog
				draft={draft}
				onClose={() => setDraft(null)}
				onConfirm={onConfirmAdd}
			/>
		</div>
	);
};

/** The source table: health per source, with the built-in one locked. */
export const SourceList: FC<{
	sources: {
		data?: SourceView[];
		isLoading: boolean;
		error: unknown;
		refetch?: () => void;
	};
	/** Name of the source whose delete is in flight, or null. */
	busyName: string | null;
	isRefreshing: boolean;
	onRefresh: () => void;
	onRemove: (source: SourceView) => void;
}> = ({ sources, busyName, isRefreshing, onRefresh, onRemove }) => {
	const rows = sources.data ?? [];
	return (
		<Card>
			<CardHeader className="flex-row items-center justify-between space-y-0">
				<CardTitle className="flex items-center gap-2">
					<Globe className="size-4" />
					{m.store_sources_title()}
				</CardTitle>
				<Button
					variant="outline"
					size="sm"
					disabled={isRefreshing}
					onClick={onRefresh}
				>
					<RefreshCw
						className={isRefreshing ? "size-4 animate-spin" : "size-4"}
					/>
					{m.store_refresh_all()}
				</Button>
			</CardHeader>
			<CardContent className="space-y-4">
				<p className="max-w-prose text-sm text-muted-foreground">
					{m.store_sources_help()}
				</p>

				<QueryState
					isLoading={sources.isLoading}
					error={sources.error}
					refetch={sources.refetch}
				>
					<Stagger gap={ROW_GAP} className="flex flex-col gap-3">
						{rows.map((s) => (
							<motion.div
								variants={ROW}
								key={s.name}
								className="flex flex-col gap-2 rounded-lg border p-3 sm:flex-row sm:items-start"
							>
								<div className="min-w-0 flex-1 space-y-1">
									<div className="flex flex-wrap items-center gap-2">
										<span className="font-medium">{s.name}</span>
										{s.builtin && (
											<Badge variant="secondary" className="gap-1">
												<Lock className="size-3" />
												{m.store_source_builtin()}
											</Badge>
										)}
										{s.signed ? (
											<Badge variant="outline" className="gap-1">
												<ShieldCheck className="size-3" />
												{m.store_source_signed()}
											</Badge>
										) : (
											<Badge
												variant="outline"
												className="gap-1 border-amber-600/40 text-amber-600 dark:border-amber-500/40 dark:text-amber-500"
											>
												<ShieldOff className="size-3" />
												{m.store_source_unsigned()}
											</Badge>
										)}
										{s.stale && (
											<Badge
												variant="outline"
												className="gap-1 border-amber-600/40 text-amber-600 dark:border-amber-500/40 dark:text-amber-500"
											>
												<AlertTriangle className="size-3" />
												{m.store_source_stale()}
											</Badge>
										)}
									</div>
									<p className="truncate font-mono text-xs text-muted-foreground">
										{s.url}
									</p>
									<p className="text-xs text-muted-foreground">
										{m.store_source_entries({ count: s.entry_count })} ·{" "}
										{m.store_source_fetched({ when: fmtFetched(s.fetched_at) })}
									</p>
									{s.error && (
										<p className="text-xs text-destructive">{s.error}</p>
									)}
								</div>
								{/* The built-in catalog gets no delete button at all — not a disabled one. */}
								{!s.builtin && (
									<Button
										variant="ghost"
										size="icon"
										aria-label={m.store_source_remove()}
										disabled={busyName === s.name}
										onClick={() => onRemove(s)}
									>
										<Trash2 className="size-4 text-destructive" />
									</Button>
								)}
							</motion.div>
						))}
					</Stagger>
				</QueryState>
			</CardContent>
		</Card>
	);
};

/** The add-source form. Reports a draft; the parent takes it through the trust dialog. */
export const AddSourceForm: FC<{
	onSubmit: (draft: SourceDraft) => void;
	isSaving: boolean;
}> = ({ onSubmit, isSaving }) => {
	const [name, setName] = useState("");
	const [url, setUrl] = useState("");
	const [publicKey, setPublicKey] = useState("");

	const handleSubmit = (e: FormEvent) => {
		e.preventDefault();
		const key = publicKey.trim();
		if (!name.trim() || !url.trim()) return;
		onSubmit({
			name: name.trim(),
			url: url.trim(),
			public_key: key ? key : undefined,
		});
	};

	return (
		<Card className="max-w-xl">
			<CardHeader>
				<CardTitle className="flex items-center gap-2">
					<Plus className="size-4" />
					{m.store_add_source_title()}
				</CardTitle>
			</CardHeader>
			<CardContent>
				<form onSubmit={handleSubmit} className="space-y-4">
					<div className="space-y-2">
						<Label htmlFor="store-source-name">
							{m.store_field_source_name()}
						</Label>
						<Input
							id="store-source-name"
							required
							autoComplete="off"
							spellCheck={false}
							value={name}
							onChange={(e) => setName(e.target.value)}
						/>
					</div>
					<div className="space-y-2">
						<Label htmlFor="store-source-url">
							{m.store_field_source_url()}
						</Label>
						<Input
							id="store-source-url"
							required
							type="url"
							inputMode="url"
							value={url}
							onChange={(e) => setUrl(e.target.value)}
						/>
					</div>
					<div className="space-y-2">
						<Label htmlFor="store-source-key">
							{m.store_field_source_key()}
						</Label>
						<Input
							id="store-source-key"
							autoComplete="off"
							spellCheck={false}
							placeholder="ed25519:…"
							value={publicKey}
							onChange={(e) => setPublicKey(e.target.value)}
						/>
						<p className="text-xs text-muted-foreground">
							{m.store_field_source_key_help()}
						</p>
					</div>
					<Button
						type="submit"
						disabled={isSaving || !name.trim() || !url.trim()}
					>
						{m.store_add_source()}
					</Button>
				</form>
			</CardContent>
		</Card>
	);
};

/** The one-time trust warning shown before a third-party catalog is written to the host. A
 * refused password keeps it open, so the operator retries without refilling the form. */
export const TrustSourceDialog: FC<{
	draft: SourceDraft | null;
	onClose: () => void;
	onConfirm: (draft: SourceDraft, password: string) => Promise<void>;
}> = ({ draft, onClose, onConfirm }) =>
	draft && (
		// Adding a source is a trust-root change: every future install rides on it, so the console
		// password is re-entered here and verified at the BFF, exactly as for a host update.
		<PasswordConfirmDialog
			open
			id="store-source-password"
			title={
				<>
					<AlertTriangle className="size-5 shrink-0 text-amber-600 dark:text-amber-500" />
					{m.store_source_trust_title()}
				</>
			}
			body={m.store_source_trust_body({ name: draft.name })}
			submitLabel={m.store_source_trust_confirm()}
			failedText={m.store_add_source_failed()}
			onSubmit={(password) => onConfirm(draft, password)}
			onClose={onClose}
		>
			<p className="rounded-md bg-muted px-3 py-2 font-mono text-xs break-all text-muted-foreground">
				{draft.url}
			</p>
			{!draft.public_key && (
				<p className="rounded-md border border-amber-600/40 bg-amber-500/10 px-3 py-2 text-sm text-amber-600 dark:border-amber-500/40 dark:text-amber-500">
					{m.store_source_trust_unsigned()}
				</p>
			)}
		</PasswordConfirmDialog>
	);
