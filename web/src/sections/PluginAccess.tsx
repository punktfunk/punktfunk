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
import type { PluginAccessSnapshot } from "@/api/gen/model/pluginAccessSnapshot";
import {
	getGetPluginAccessQueryKey,
	useDecidePluginAccess,
	useGetPluginAccess,
} from "@/api/gen/plugin-access/plugin-access";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";

export type AccessDecision = "allow" | "deny" | "forget";
export type DecideAccess = (paths: string[], decision: AccessDecision) => void;

export const usePluginAccess = () => {
	const qc = useQueryClient();
	const access = useGetPluginAccess();
	const decide = useDecidePluginAccess();
	const onDecide = async (
		plugin: string,
		paths: string[],
		decision: AccessDecision,
	) => {
		try {
			for (const path of paths) {
				const snapshot = await decide.mutateAsync({
					plugin,
					data: { path, decision },
				});
				qc.setQueryData<PluginAccessSnapshot[]>(
					getGetPluginAccessQueryKey(),
					(rows = []) => [
						...rows.filter((row) => row.plugin !== plugin),
						snapshot,
					],
				);
			}
		} catch {
			toast.error(m.plugin_access_decision_failed());
		}
	};
	return { access, busy: decide.isPending, onDecide };
};

const Mode: FC<{ write: boolean }> = ({ write }) => (
	<span className="text-xs text-muted-foreground">
		{write ? m.plugin_access_read_write() : m.plugin_access_read_only()}
	</span>
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
											{row.path}
										</div>
										<Mode write={row.write} />
									</>
								)}
								{row.reason && (
									<p className="mt-1 text-xs text-muted-foreground">
										{row.reason}
									</p>
								)}
							</div>
						</div>
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
	if (count === 0) return null;
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
				{m.plugin_access_folders({ count })}
			</button>
			{open && (
				<div className="mt-1 space-y-0.5 pl-5">
					{access.grants.map((grant) => (
						<div key={grant.path} className="flex items-center gap-2 text-xs">
							<FolderLock className="size-3.5 shrink-0 text-muted-foreground" />
							<div className="min-w-0 flex-1">
								<span className="break-all font-mono">{grant.path}</span>{" "}
								<Mode write={grant.write} />
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
