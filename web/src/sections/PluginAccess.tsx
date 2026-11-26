import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import {
	AlertTriangle,
	ChevronRight,
	Download,
	FolderLock,
	Network,
	RotateCcw,
	Save,
	Trash2,
} from "lucide-react";
import { type FC, useId, useState } from "react";
import { useGetEmulators } from "@/api/gen/emulators/emulators";
import type { AccessWalk } from "@/api/gen/model/accessWalk";
import type { DecideRequest } from "@/api/gen/model/decideRequest";
import type { EmulatorStatus } from "@/api/gen/model/emulatorStatus";
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
import { Input } from "@/components/ui/input";
import { Spinner } from "@/components/ui/spinner";
import { usePerson } from "@/lib/person";
import { isStringArray, useLocalPref } from "@/lib/prefs";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";

export type AccessDecision = "allow" | "deny" | "forget";
export type DecideAccess = (paths: string[], decision: AccessDecision) => void;

/**
 * The access rows and their decisions. **Allow** asks the operator for the console password in
 * `dialog`, which the page mounts; a person's own seat host proves them, so they aren't asked.
 * **Don't allow** and **Forget** only narrow and never ask. A person's rows are their seat
 * host's: only their own desktop's plugins ask there.
 */
export const usePluginAccess = () => {
	const qc = useQueryClient();
	const person = usePerson() !== null;
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
	const emulators = useGetEmulators({
		query: {
			enabled: !!access.data?.some((row) =>
				row.pending.some((p) => p.emulator),
			),
		},
	});
	// The emulator or cores a yes installs first, which the dialog names instead of access.
	const install =
		allowing &&
		installOf(access.data, allowing.plugin, allowing.paths, (id) =>
			emulatorName(emulators.data, id),
		);
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
		if (decision === "allow" && !person) {
			setAllowing({ plugin, paths });
			return;
		}
		try {
			await run(plugin, paths, decision);
		} catch {
			toast.error(
				paths.some(isPortPath)
					? m.plugin_access_port_failed()
					: m.plugin_access_decision_failed(),
			);
		}
	};
	const dialog = (
		<PasswordConfirmDialog
			open={allowing !== null}
			id="plugin-access-password"
			title={
				install
					? m.plugin_access_install_confirm_title({ name: install })
					: m.plugin_access_confirm_title()
			}
			body={
				install
					? m.plugin_access_install_confirm_body({ name: install })
					: m.plugin_access_confirm_body()
			}
			submitLabel={
				install ? m.plugin_access_install() : m.plugin_access_allow()
			}
			busyLabel={
				install ? m.plugin_access_installing() : m.plugin_access_allowing()
			}
			failedText={
				allowing?.paths.some(isPortPath)
					? m.plugin_access_port_failed()
					: m.plugin_access_decision_failed()
			}
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

/** An emulator's name from the host's list, else its catalog id. */
const emulatorName = (list: EmulatorStatus[] | undefined, id: string) =>
	list?.find((e) => e.id === id)?.name ?? id;

/** What a yes on `plugin`'s `paths` installs first: an emulator's name, or the cores'. */
const installOf = (
	rows: PluginAccessSnapshot[] | undefined,
	plugin: string,
	paths: string[],
	name: (id: string) => string,
): string | undefined => {
	const asked =
		rows
			?.find((row) => row.plugin === plugin)
			?.pending.filter((row) => paths.includes(row.path)) ?? [];
	const emulator = asked.find((row) => row.emulator)?.emulator;
	const cores = asked.flatMap((row) => (row.core ? [row.core] : []));
	return emulator ? name(emulator) : cores.join(", ") || undefined;
};

/** A port on this computer as the host spells it in a row and takes it in a decision. */
const portPath = (port: number) => `127.0.0.1:${port}`;
const isPortPath = (path: string) => path.startsWith("127.0.0.1:");

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

/** A folder whose access is still changing, which takes minutes on a large Windows folder, or
 * the install a row asked for, with its download once its size is known. */
const WalkLine: FC<{ walk: AccessWalk; named?: boolean }> = ({
	walk,
	named,
}) => (
	<div className="flex items-center gap-2 text-xs text-muted-foreground">
		<Spinner className="size-3.5 shrink-0" />
		<span className="min-w-0 break-all">
			{named && <span className="font-mono">{walk.path} </span>}
			{walk.installing
				? walk.percent != null
					? m.plugin_access_walk_download({ percent: walk.percent })
					: m.plugin_access_walk_install({ seconds: walk.seconds })
				: walk.removing
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
			(row) =>
				!row.write && !row.emulator && !row.core && !row.saves && !row.port,
		);
	// An emulator row names the catalog id; the list gives it its name.
	const emulators = useGetEmulators({
		query: { enabled: access.pending.some((row) => !!row.emulator) },
	});
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
	const failedAt = (path: string) =>
		access.failed?.find((failure) => failure.path === path);
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
							) : row.port ? (
								<Network className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
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
														name: emulatorName(
															emulators.data,
															row.emulator ?? "",
														),
													})}
										</div>
										<div className="break-all font-mono text-xs text-muted-foreground">
											{row.path}
										</div>
									</>
								) : row.port ? (
									<>
										<div className="text-sm font-medium">
											{m.plugin_access_port({ port: row.port })}
										</div>
										<p className="text-xs text-muted-foreground">
											{m.plugin_access_port_note()}
										</p>
										{row.listening !== undefined && (
											<p className="text-xs text-muted-foreground">
												{!row.listening
													? m.plugin_access_port_idle()
													: row.listener
														? m.plugin_access_port_listener({
																name: row.listener,
															})
														: m.plugin_access_port_listener_other()}
											</p>
										)}
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
										{/* The folder the grant opens; a link the plugin asked through only below it. */}
										<div className="break-all font-mono text-xs">
											{row.path}
										</div>
										{row.spelled && (
											<div className="break-all text-xs text-muted-foreground">
												{m.plugin_access_via_link({ path: row.spelled })}
											</div>
										)}
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
						{failedAt(row.path) && !walkAt(row.path) && (
							<p className="mt-2 break-words text-xs text-destructive">
								{m.plugin_access_install_failed({
									name:
										cores.join(", ") ||
										emulatorName(emulators.data, row.emulator ?? ""),
									cause: failedAt(row.path)?.error ?? "",
								})}
							</p>
						)}
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

/** The operator's own way to let a plugin reach a server on this computer: a port and Allow,
 * with no request from the plugin. Folded like the row's other access lines until asked for.
 * The host refuses a port of its own. */
export const AllowPort: FC<{ busy: boolean; onDecide: DecideAccess }> = ({
	busy,
	onDecide,
}) => {
	const id = useId();
	const [open, setOpen] = useState(false);
	const [value, setValue] = useState("");
	const port = Number(value);
	const valid = Number.isInteger(port) && port >= 1 && port <= 65535;
	return (
		<div className="mt-1">
			<button
				type="button"
				aria-expanded={open}
				onClick={() => setOpen((v) => !v)}
				className="flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground"
			>
				<ChevronRight
					className={cn("size-3.5 transition-transform", open && "rotate-90")}
				/>
				{m.plugin_access_port_add()}
			</button>
			{open && (
				<form
					className="mt-2 flex items-center gap-2 pl-5"
					onSubmit={(e) => {
						e.preventDefault();
						if (!valid) return;
						onDecide([portPath(port)], "allow");
						setValue("");
						setOpen(false);
					}}
				>
					<label htmlFor={id} className="sr-only">
						{m.plugin_access_port_add()}
					</label>
					{/* `!`: the field's own height is a theme token a plain class does not displace. */}
					<Input
						id={id}
						inputMode="numeric"
						className="h-8! w-24 font-mono"
						value={value}
						onChange={(e) => setValue(e.target.value.replace(/\D/g, ""))}
					/>
					<Button type="submit" size="sm" disabled={busy || !valid}>
						{m.plugin_access_allow()}
					</Button>
				</form>
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
	const [portsOpen, setPortsOpen] = useState(false);
	const ports = access.ports ?? [];
	const deniedPorts = access.denied_ports ?? [];
	const portCount = ports.length + deniedPorts.length;
	const count = access.grants.length + access.denied.length;
	// A walk on a pending row shows on that row.
	const walks = (access.walks ?? []).filter(
		(walk) => !access.pending.some((row) => row.path === walk.path),
	);
	if (count === 0 && portCount === 0 && walks.length === 0) return null;
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
			{portCount > 0 && (
				<button
					type="button"
					aria-expanded={portsOpen}
					onClick={() => setPortsOpen((v) => !v)}
					className="flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground"
				>
					<ChevronRight
						className={cn(
							"size-3.5 transition-transform",
							portsOpen && "rotate-90",
						)}
					/>
					{m.plugin_access_ports({ count: portCount })}
				</button>
			)}
			{portsOpen && (
				<div className="mt-1 space-y-0.5 pl-5">
					{ports.map((port) => (
						<div key={port} className="flex items-center gap-2 text-xs">
							<Network className="size-3.5 shrink-0 text-muted-foreground" />
							<span className="min-w-0 flex-1 font-mono">{portPath(port)}</span>
							<Button
								variant="ghost"
								size="icon"
								aria-label={m.common_remove()}
								title={m.common_remove()}
								disabled={busy}
								onClick={() => onDecide([portPath(port)], "forget")}
							>
								<Trash2 className="size-3.5 text-destructive" />
							</Button>
						</div>
					))}
					{deniedPorts.map((port) => (
						<div key={port} className="flex items-center gap-2 text-xs">
							<div className="min-w-0 flex-1 font-mono text-muted-foreground">
								{portPath(port)}
							</div>
							<Button
								size="sm"
								variant="ghost"
								disabled={busy}
								onClick={() => onDecide([portPath(port)], "forget")}
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
