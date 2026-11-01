import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import {
	AlertTriangle,
	ChevronRight,
	Download,
	FolderLock,
	RotateCcw,
	Save,
	Trash2,
} from "lucide-react";
import { type FC, useState } from "react";
import { useGetEmulators } from "@/api/gen/emulators/emulators";
import type { AccessWalk } from "@/api/gen/model/accessWalk";
import type { DecideRequest } from "@/api/gen/model/decideRequest";
import type { PluginAccessSnapshot } from "@/api/gen/model/pluginAccessSnapshot";
import {
	getGetPluginAccessQueryKey,
	useDecidePluginAccess,
	useGetPluginAccess,
} from "@/api/gen/plugin-access/plugin-access";
import { usePlugins } from "@/api/plugins";
import { PasswordConfirmDialog } from "@/components/password-confirm";
import { Button } from "@/components/ui/button";
import {
	Dialog,
	DialogContent,
	DialogDescription,
	DialogFooter,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { Spinner } from "@/components/ui/spinner";
import { isStringArray, useLocalPref } from "@/lib/prefs";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";

export type AccessDecision = "allow" | "deny" | "forget";
export type DecideAccess = (paths: string[], decision: AccessDecision) => void;

/**
 * The access rows and their decisions. **Allow** asks for the console password in `dialog`, which
 * the page mounts; **Don't allow** and **Forget** only narrow and don't ask.
 */
export const usePluginAccess = () => {
	const qc = useQueryClient();
	// A large Windows folder outlasts the decision that started it; the count comes from here.
	const access = useGetPluginAccess({
		query: {
			refetchInterval: (query) =>
				query.state.data?.some((row) => row.walks?.length) ? 1000 : false,
		},
	});
	const decide = useDecidePluginAccess();
	const [allowing, setAllowing] = useState<{
		plugin: string;
		paths: string[];
	} | null>(null);
	const run = async (
		plugin: string,
		paths: string[],
		decision: AccessDecision,
		password?: string,
	) => {
		for (const path of paths) {
			// The console's server reads the password from the body and strips it.
			const data = { path, decision, password } as DecideRequest;
			const snapshot = await decide.mutateAsync({ plugin, data });
			qc.setQueryData<PluginAccessSnapshot[]>(
				getGetPluginAccessQueryKey(),
				(rows = []) => [
					...rows.filter((row) => row.plugin !== plugin),
					snapshot,
				],
			);
		}
	};
	const onDecide = async (
		plugin: string,
		paths: string[],
		decision: AccessDecision,
	) => {
		if (decision === "allow") {
			setAllowing({ plugin, paths });
			return;
		}
		try {
			await run(plugin, paths, decision);
		} catch {
			toast.error(m.plugin_access_decision_failed());
		}
	};
	const dialog = (
		<PasswordConfirmDialog
			open={allowing !== null}
			id="plugin-access-password"
			title={m.plugin_access_confirm_title()}
			body={m.plugin_access_confirm_body()}
			submitLabel={m.plugin_access_allow()}
			busyLabel={m.plugin_access_allowing()}
			failedText={m.plugin_access_decision_failed()}
			onSubmit={async (password) => {
				if (allowing)
					await run(allowing.plugin, allowing.paths, "allow", password);
			}}
			onClose={() => setAllowing(null)}
		/>
	);
	return {
		access,
		busy: decide.isPending,
		asking: allowing !== null,
		onDecide,
		dialog,
	};
};

/** Folders this browser put off with Later, as `plugin\npath`. */
const NONE: string[] = [];
const laterKey = (plugin: string, path: string) => `${plugin}\n${path}`;

/** The first plugin with a request this browser hasn't put off. */
export const nextAsk = (
	rows: PluginAccessSnapshot[],
	later: string[],
): PluginAccessSnapshot | undefined =>
	rows.find((row) =>
		row.pending.some((p) => !later.includes(laterKey(row.plugin, p.path))),
	);

/** `later` with every request of `ask` added and any no longer pending dropped. */
export const putOff = (
	rows: PluginAccessSnapshot[],
	later: string[],
	ask: PluginAccessSnapshot,
): string[] => {
	const pending = new Set(
		rows.flatMap((row) => row.pending.map((p) => laterKey(row.plugin, p.path))),
	);
	const put = ask.pending.map((p) => laterKey(ask.plugin, p.path));
	return [...new Set([...later.filter((k) => pending.has(k)), ...put])];
};

/**
 * Asks on any page as soon as a plugin requests a folder, so a new source's games show up
 * without a visit to its source line. Later keeps the request there and stops this browser
 * asking about those folders again.
 */
export const AccessPrompt: FC = () => {
	const { access, busy, asking, onDecide, dialog } = usePluginAccess();
	const plugins = usePlugins();
	const [later, setLater] = useLocalPref(
		"pf-access-later",
		NONE,
		isStringArray,
	);
	const rows = access.data ?? [];
	const ask = nextAsk(rows, later);
	const postpone = () => ask && setLater(putOff(rows, later, ask));
	const name =
		plugins.data?.find((p) => p.id === ask?.plugin)?.title ?? ask?.plugin;
	return (
		<>
			<Dialog open={!!ask && !asking} onOpenChange={(o) => !o && postpone()}>
				<DialogContent>
					<DialogHeader>
						<DialogTitle>
							{m.plugin_access_prompt_title({ plugin: name ?? "" })}
						</DialogTitle>
						<DialogDescription>
							{m.plugin_access_prompt_body()}
						</DialogDescription>
					</DialogHeader>
					{ask && (
						<PendingAccess
							access={ask}
							busy={busy}
							onDecide={(paths, decision) =>
								onDecide(ask.plugin, paths, decision)
							}
						/>
					)}
					<DialogFooter>
						<Button variant="outline" onClick={postpone}>
							{m.plugin_access_later()}
						</Button>
					</DialogFooter>
				</DialogContent>
			</Dialog>
			{dialog}
		</>
	);
};

const Mode: FC<{ write: boolean; files?: boolean }> = ({ write, files }) => (
	<span className="text-xs text-muted-foreground">
		{write
			? m.plugin_access_read_write()
			: files
				? m.plugin_access_read_files()
				: m.plugin_access_read_only()}
	</span>
);

/** A folder whose access is still changing, which takes minutes on a large Windows folder. */
const WalkLine: FC<{ walk: AccessWalk; named?: boolean }> = ({
	walk,
	named,
}) => (
	<div className="flex items-center gap-2 text-xs text-muted-foreground">
		<Spinner className="size-3.5 shrink-0" />
		<span className="min-w-0 break-all">
			{named && <span className="font-mono">{walk.path} </span>}
			{walk.removing
				? m.plugin_access_walk_back({ seconds: walk.seconds })
				: m.plugin_access_walk({ seconds: walk.seconds })}
		</span>
	</div>
);

export const PendingAccess: FC<{
	access: PluginAccessSnapshot;
	busy: boolean;
	onDecide: DecideAccess;
}> = ({ access, busy, onDecide }) => {
	const allowAll =
		access.pending.length > 1 &&
		access.pending.every(
			(row) => !row.write && !row.emulator && !row.core && !row.saves,
		);
	// An emulator row names the catalog id; the list gives it its name.
	const emulators = useGetEmulators({
		query: { enabled: access.pending.some((row) => !!row.emulator) },
	});
	const emulatorName = (id: string) =>
		emulators.data?.find((e) => e.id === id)?.name ?? id;
	// The host answers a folder, not a row: a yes on RetroArch's cores folder installs every
	// core asked for there, so the rows on one folder show as one.
	const rows = [
		...new Map(access.pending.map((row) => [row.path, row])).values(),
	];
	const coresAt = (path: string) =>
		access.pending.flatMap((row) =>
			row.path === path && row.core ? [row.core] : [],
		);
	const walkAt = (path: string) =>
		access.walks?.find((walk) => walk.path === path);
	const [asked, setAsked] = useState<string>();
	return (
		<div className="mt-3 space-y-2 border-t pt-3">
			{rows.map((row) => {
				const cores = coresAt(row.path);
				const install = !!row.emulator || cores.length > 0;
				return (
					<div
						key={row.path}
						className={
							row.write
								? "rounded-md border border-amber-600/40 bg-amber-500/5 p-3"
								: "rounded-md border p-3"
						}
					>
						<div className="flex items-start gap-2">
							{install ? (
								<Download className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
							) : row.saves ? (
								<Save className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
							) : row.write ? (
								<AlertTriangle className="mt-0.5 size-4 shrink-0 text-amber-600 dark:text-amber-500" />
							) : (
								<FolderLock className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
							)}
							<div className="min-w-0 flex-1">
								{install ? (
									<>
										<div className="text-sm font-medium">
											{cores.length > 0
												? m.plugin_access_install_core({
														name: cores.join(", "),
													})
												: m.plugin_access_install_emulator({
														name: emulatorName(row.emulator ?? ""),
													})}
										</div>
										<div className="break-all font-mono text-xs text-muted-foreground">
											{row.path}
										</div>
									</>
								) : row.saves ? (
									<>
										<div className="text-sm font-medium">
											{m.plugin_access_saves()}
										</div>
										<p className="text-xs text-muted-foreground">
											{m.plugin_access_saves_note()}
										</p>
										<div className="break-all font-mono text-xs text-muted-foreground">
											{row.path}
										</div>
									</>
								) : (
									<>
										<div className="break-all font-mono text-xs">
											{row.spelled ?? row.path}
										</div>
										<Mode write={row.write} files={row.files} />
									</>
								)}
								{row.reason && (
									<p className="mt-1 text-xs text-muted-foreground">
										{row.reason}
									</p>
								)}
							</div>
						</div>
						{walkAt(row.path) ? (
							<div className="mt-2">
								<WalkLine walk={walkAt(row.path) as AccessWalk} />
							</div>
						) : (
							<div className="mt-2 flex flex-wrap justify-end gap-2">
								<Button
									size="sm"
									variant="outline"
									disabled={busy}
									onClick={() => onDecide([row.path], "deny")}
								>
									{install
										? m.plugin_access_not_now()
										: m.plugin_access_dont_allow()}
								</Button>
								<Button
									size="sm"
									disabled={busy}
									onClick={() => {
										setAsked(row.path);
										onDecide([row.path], "allow");
									}}
								>
									{install
										? busy && asked === row.path
											? m.plugin_access_installing()
											: m.plugin_access_install()
										: m.plugin_access_allow()}
								</Button>
							</div>
						)}
					</div>
				);
			})}
			{allowAll && (
				<div className="flex justify-end">
					<Button
						size="sm"
						variant="outline"
						disabled={busy}
						onClick={() =>
							onDecide(
								access.pending.map((row) => row.path),
								"allow",
							)
						}
					>
						{m.plugin_access_allow_all()}
					</Button>
				</div>
			)}
		</div>
	);
};

export const RecordedAccess: FC<{
	access: PluginAccessSnapshot;
	busy: boolean;
	onDecide: DecideAccess;
}> = ({ access, busy, onDecide }) => {
	const [open, setOpen] = useState(false);
	const count = access.grants.length + access.denied.length;
	// A walk on a pending row shows on that row.
	const walks = (access.walks ?? []).filter(
		(walk) => !access.pending.some((row) => row.path === walk.path),
	);
	if (count === 0 && walks.length === 0) return null;
	return (
		<div className="mt-1 space-y-1">
			{walks.map((walk) => (
				<WalkLine key={walk.path} walk={walk} named />
			))}
			{count > 0 && (
				<button
					type="button"
					aria-expanded={open}
					onClick={() => setOpen((v) => !v)}
					className="flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground"
				>
					<ChevronRight
						className={cn("size-3.5 transition-transform", open && "rotate-90")}
					/>
					{m.plugin_access_folders({ count })}
				</button>
			)}
			{open && (
				<div className="mt-1 space-y-0.5 pl-5">
					{access.grants.map((grant) => (
						<div key={grant.path} className="flex items-center gap-2 text-xs">
							<FolderLock className="size-3.5 shrink-0 text-muted-foreground" />
							<div className="min-w-0 flex-1">
								<span className="break-all font-mono">{grant.path}</span>{" "}
								<Mode write={grant.write} files={grant.files} />
							</div>
							<Button
								variant="ghost"
								size="icon"
								aria-label={m.common_remove()}
								title={m.common_remove()}
								disabled={busy}
								onClick={() => onDecide([grant.path], "forget")}
							>
								<Trash2 className="size-3.5 text-destructive" />
							</Button>
						</div>
					))}
					{access.denied.map((path) => (
						<div key={path} className="flex items-center gap-2 text-xs">
							<div className="min-w-0 flex-1 break-all font-mono text-muted-foreground">
								{path}
							</div>
							<Button
								size="sm"
								variant="ghost"
								disabled={busy}
								onClick={() => onDecide([path], "forget")}
							>
								<RotateCcw className="size-3.5" />
								{m.plugin_access_ask_again()}
							</Button>
						</div>
					))}
				</div>
			)}
		</div>
	);
};
