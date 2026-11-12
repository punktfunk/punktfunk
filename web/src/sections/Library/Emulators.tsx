import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { ChevronDown, Cpu, Download } from "lucide-react";
import { type FC, type FormEvent, useState } from "react";
import {
	getGetEmulatorsQueryKey,
	useAdoptEmulator,
	useGetEmulators,
	useInstallEmulator,
	useRemoveEmulator,
} from "@/api/gen/emulators/emulators";
import type { AdoptRequest } from "@/api/gen/model/adoptRequest";
import type { EmulatorStatus } from "@/api/gen/model/emulatorStatus";
import { PasswordConfirmDialog } from "@/components/password-confirm";
import { ROW_GAP, Stagger } from "@/components/stagger";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { MenuItem, MenuSeparator, RowMenu } from "@/components/ui/menu";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";
import { SourceGroup, SourceItem } from "./AddSource";

/** One line on where the emulator is, or why it is not. */
const Where: FC<{ e: EmulatorStatus }> = ({ e }) => {
	if (e.managed) {
		return (
			<>
				{m.emulators_managed()}
				{e.managed.version ? ` · ${e.managed.version}` : ""}
			</>
		);
	}
	const found = e.detected[0];
	if (found) {
		return <span className="font-mono">{found.exe}</span>;
	}
	return (
		<>{e.offered ? m.emulators_not_installed() : m.emulators_not_offered()}</>
	);
};

/**
 * The operator's own copy, one no rule finds: its program's path, adopted on Add. The host runs
 * that program for the emulator's games, so Add asks for the console password first.
 */
const Adopt: FC<{ id: string; onDone: () => void }> = ({ id, onDone }) => {
	const [exe, setExe] = useState("");
	const [asking, setAsking] = useState(false);
	const adopt = useAdoptEmulator();
	const submit = (e: FormEvent) => {
		e.preventDefault();
		if (exe.trim()) setAsking(true);
	};
	return (
		<>
			<form onSubmit={submit} className="flex w-full gap-2">
				<Input
					value={exe}
					onChange={(e) => setExe(e.target.value)}
					placeholder={m.emulators_adopt_path()}
					aria-label={m.emulators_adopt_path()}
					className="font-mono text-xs"
				/>
				<Button
					size="input"
					type="submit"
					disabled={!exe.trim() || adopt.isPending}
				>
					{m.emulators_adopt_add()}
				</Button>
			</form>
			<PasswordConfirmDialog
				open={asking}
				id="emulator-adopt-password"
				title={m.emulators_adopt_confirm_title()}
				body={m.emulators_adopt_confirm_body()}
				submitLabel={m.emulators_adopt_add()}
				failedText={m.emulators_adopt_failed()}
				onSubmit={async (password) => {
					// The console's server reads the password from the body and strips it.
					const data = { exe: exe.trim(), password } as AdoptRequest;
					await adopt.mutateAsync({ id, data });
					onDone();
				}}
				onClose={() => setAsking(false)}
			/>
		</>
	);
};

/**
 * Every emulator the host knows: installed by punktfunk or found on the box first, the rest one
 * tap away. Installing and pointing at the operator's own copy are the operator's acts here; a
 * plugin's own ask lands in its source's Access rows instead. They are the owner's library's, so
 * only its view shows them.
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
	const [showAbsent, setShowAbsent] = useState(false);
	const list = rows.data ?? [];
	if (list.length === 0) return null;
	const here = (e: EmulatorStatus) => !!e.managed || e.detected.length > 0;
	const present = list.filter(here);
	const absent = list.filter((e) => !here(e));
	// With nothing on the box, the list is the offer itself.
	const shown =
		showAbsent || present.length === 0 ? [...present, ...absent] : present;
	return (
		<SourceGroup
			icon={<Cpu className="size-4" />}
			title={m.emulators_title()}
			description={m.emulators_description()}
		>
			<Stagger gap={ROW_GAP} className="divide-y">
				{shown.map((e) => (
					<SourceItem
						key={e.id}
						title={e.name}
						meta={e.platforms.join(", ")}
						detail={<Where e={e} />}
						hint={`${e.platforms.join(", ")}\n${e.detected[0]?.exe ?? ""}`.trim()}
						actions={
							<>
								{!here(e) && e.offered && (
									<Button
										size="sm"
										disabled={busy}
										onClick={() => install.mutate({ id: e.id })}
									>
										<Download className="size-3.5" />
										{m.emulators_install()}
									</Button>
								)}
								<RowMenu label={m.common_more_actions()} disabled={busy}>
									{here(e) && e.offered && (
										<MenuItem onSelect={() => install.mutate({ id: e.id })}>
											{e.managed
												? m.emulators_reinstall()
												: m.emulators_install()}
										</MenuItem>
									)}
									<MenuItem
										onSelect={() =>
											setAdopting(adopting === e.id ? undefined : e.id)
										}
									>
										{m.emulators_adopt()}
									</MenuItem>
									{e.managed && (
										<>
											<MenuSeparator />
											<MenuItem
												destructive
												onSelect={() =>
													remove.mutate({ id: e.id, data: { purge: false } })
												}
											>
												{m.emulators_remove()}
											</MenuItem>
										</>
									)}
								</RowMenu>
							</>
						}
					>
						{adopting === e.id && (
							<div className="mt-2">
								<Adopt
									id={e.id}
									onDone={() => {
										setAdopting(undefined);
										void refresh();
									}}
								/>
							</div>
						)}
					</SourceItem>
				))}
			</Stagger>
			{present.length > 0 && absent.length > 0 && (
				<Button
					variant="ghost"
					size="sm"
					className="-ml-3"
					aria-expanded={showAbsent}
					onClick={() => setShowAbsent((v) => !v)}
				>
					<ChevronDown
						className={cn(
							"size-4 transition-transform",
							showAbsent && "rotate-180",
						)}
					/>
					{showAbsent
						? m.emulators_hide_absent()
						: m.emulators_show_absent({ count: absent.length })}
				</Button>
			)}
		</SourceGroup>
	);
};
