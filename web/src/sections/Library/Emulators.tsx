import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { Cpu, Download, FolderSearch, Trash2 } from "lucide-react";
import { type FC, type FormEvent, useState } from "react";
import {
	getGetEmulatorsQueryKey,
	useAdoptEmulator,
	useGetEmulators,
	useInstallEmulator,
	useRemoveEmulator,
} from "@/api/gen/emulators/emulators";
import type { EmulatorStatus } from "@/api/gen/model/emulatorStatus";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { m } from "@/paraglide/messages";
import { SourceGroup } from "./AddSource";

/** One line on where the emulator is, or why it is not. */
const Where: FC<{ e: EmulatorStatus }> = ({ e }) => {
	if (e.managed) {
		return (
			<span className="text-xs text-muted-foreground">
				{m.emulators_managed()}
				{e.managed.version ? ` · ${e.managed.version}` : ""}
			</span>
		);
	}
	const found = e.detected[0];
	if (found) {
		return (
			<span className="break-all font-mono text-xs text-muted-foreground">
				{found.exe}
			</span>
		);
	}
	return (
		<span className="text-xs text-muted-foreground">
			{e.offered ? m.emulators_not_installed() : m.emulators_not_offered()}
		</span>
	);
};

/** The operator's own copy, one no rule finds: its program's path, adopted on Add. */
const Adopt: FC<{ id: string; onDone: () => void }> = ({ id, onDone }) => {
	const [exe, setExe] = useState("");
	const adopt = useAdoptEmulator({
		mutation: {
			onSuccess: onDone,
			onError: () => toast.error(m.emulators_adopt_failed()),
		},
	});
	const submit = (e: FormEvent) => {
		e.preventDefault();
		if (exe.trim()) adopt.mutate({ id, data: { exe: exe.trim() } });
	};
	return (
		<form onSubmit={submit} className="flex w-full gap-2">
			<Input
				value={exe}
				onChange={(e) => setExe(e.target.value)}
				placeholder={m.emulators_adopt_path()}
				aria-label={m.emulators_adopt_path()}
				className="font-mono text-xs"
			/>
			<Button size="sm" type="submit" disabled={!exe.trim() || adopt.isPending}>
				{m.emulators_adopt_add()}
			</Button>
		</form>
	);
};

/**
 * Every emulator the host knows: installed by punktfunk, found on the box, or neither. Installing
 * and pointing at the operator's own copy are the operator's acts here; a plugin's own ask lands
 * in its source's Access rows instead.
 */
export const EmulatorsCard: FC = () => {
	const qc = useQueryClient();
	const rows = useGetEmulators();
	const refresh = () =>
		qc.invalidateQueries({ queryKey: getGetEmulatorsQueryKey() });
	const install = useInstallEmulator({
		mutation: {
			onSuccess: refresh,
			onError: () => toast.error(m.emulators_install_failed()),
		},
	});
	const remove = useRemoveEmulator({
		mutation: {
			onSuccess: refresh,
			onError: () => toast.error(m.emulators_remove_failed()),
		},
	});
	const busy = install.isPending || remove.isPending;
	const [adopting, setAdopting] = useState<string>();
	const list = rows.data ?? [];
	if (list.length === 0) return null;
	return (
		<SourceGroup
			icon={<Cpu className="size-4" />}
			title={m.emulators_title()}
			description={m.emulators_description()}
		>
			<div className="divide-y">
				{list.map((e) => (
					<div
						key={e.id}
						className="flex flex-wrap items-center gap-x-3 gap-y-1 py-3"
					>
						<div className="min-w-0 flex-1">
							<div className="text-sm font-medium">{e.name}</div>
							<div className="text-xs text-muted-foreground">
								{e.platforms.join(", ")}
							</div>
							<Where e={e} />
						</div>
						<div className="flex gap-2">
							{e.managed ? (
								<Button
									size="sm"
									variant="outline"
									disabled={busy}
									onClick={() =>
										remove.mutate({ id: e.id, data: { purge: false } })
									}
								>
									<Trash2 className="size-3.5" />
									{m.emulators_remove()}
								</Button>
							) : null}
							{e.offered ? (
								<Button
									size="sm"
									variant={e.managed ? "outline" : "default"}
									disabled={busy}
									onClick={() => install.mutate({ id: e.id })}
								>
									<Download className="size-3.5" />
									{e.managed ? m.emulators_reinstall() : m.emulators_install()}
								</Button>
							) : null}
							<Button
								size="sm"
								variant="ghost"
								onClick={() =>
									setAdopting(adopting === e.id ? undefined : e.id)
								}
							>
								<FolderSearch className="size-3.5" />
								{m.emulators_adopt()}
							</Button>
						</div>
						{adopting === e.id && (
							<Adopt
								id={e.id}
								onDone={() => {
									setAdopting(undefined);
									void refresh();
								}}
							/>
						)}
					</div>
				))}
			</div>
		</SourceGroup>
	);
};
