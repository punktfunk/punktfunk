import type { FC } from "react";
import type { AudioSessions } from "@/api/gen/model/audioSessions";
import type { LaunchSpec } from "@/api/gen/model/launchSpec";
import { Checkbox } from "@/components/ui/checkbox";
import { Label } from "@/components/ui/label";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { m } from "@/paraglide/messages";
import { storeLabel } from "../../helpers";
import { storeOf } from "../../overrides";
import { useSourceNames } from "../../Sources";
import { StreamControls } from "../../Stream";
import { Group, ReadRow, TextField } from "../fields";
import type { TabProps } from "./types";

/** One line naming how a launch runs. */
export function describeLaunch(launch: LaunchSpec | null | undefined): string {
	if (!launch) return m.library_entry_launch_none();
	switch (launch.kind) {
		case "steam_appid":
			return m.library_entry_launch_steam({ id: launch.value });
		case "command":
			return m.library_entry_launch_command({ command: launch.value });
		default:
			return m.library_entry_launch_exec({ name: launch.value });
	}
}

/** The host's stream settings for this game. A store of its own: each control saves, apart from
 * the draft and Save, so a provider's title has them too. */
const StreamGroup: FC<{ id: string }> = ({ id }) => {
	const nameOf = useSourceNames();
	return (
		<Group title={m.library_stream_title()} help={m.library_stream_hint()}>
			<StreamControls
				kind="game"
				id={id}
				from={storeLabel(storeOf(id), nameOf)}
			/>
		</Group>
	);
};

/** Launch, role, process hints, audio, the prep commands as read, and the stream settings. */
export const LaunchTab: FC<TabProps> = ({ draft, set, readOnly, entry }) => {
	const stream = entry && <StreamGroup id={entry.id} />;
	if (readOnly) {
		return (
			<>
				<Group>
					<div className="grid gap-4 @md:grid-cols-2">
						<ReadRow
							label={m.library_entry_launch_title()}
							value={describeLaunch(entry?.launch)}
						/>
						<ReadRow
							label={m.library_entry_role()}
							value={
								entry?.role === "launcher"
									? m.library_entry_role_launcher()
									: m.library_entry_role_game()
							}
						/>
					</div>
				</Group>
				{stream}
			</>
		);
	}
	const kept = !draft.command.trim() && draft.launch?.kind !== "command";
	return (
		<>
			<Group>
				{kept && draft.launch && (
					<p className="rounded-md border bg-muted/40 px-3 py-2 text-sm">
						{describeLaunch(draft.launch)}{" "}
						<span className="text-muted-foreground">
							{m.library_entry_launch_replaced()}
						</span>
					</p>
				)}
				<TextField
					id="command"
					label={m.library_field_command()}
					value={draft.command}
					onChange={(v) => set("command", v)}
					help={m.library_field_command_help()}
				/>
				{/* A launcher entry moves to the Launchers rail; it launches like any other. */}
				<div className="space-y-2">
					<div className="flex items-center gap-2">
						<Checkbox
							id="entry-isLauncher"
							checked={draft.isLauncher}
							onCheckedChange={(next) => set("isLauncher", next === true)}
						/>
						<Label htmlFor="entry-isLauncher">{m.library_field_role()}</Label>
					</div>
					<p className="text-xs text-muted-foreground">
						{m.library_field_role_help()}
					</p>
				</div>
			</Group>
			{stream}
			<Group title={m.library_process_legend()} help={m.library_process_help()}>
				<div className="grid gap-4 @lg:grid-cols-3">
					<TextField
						id="exe"
						label={m.library_field_exe()}
						value={draft.exe}
						onChange={(v) => set("exe", v)}
						help={m.library_field_exe_help()}
					/>
					<TextField
						id="installDir"
						label={m.library_field_install_dir()}
						value={draft.installDir}
						onChange={(v) => set("installDir", v)}
						help={m.library_field_install_dir_help()}
					/>
					<TextField
						id="processName"
						label={m.library_field_process_name()}
						value={draft.processName}
						onChange={(v) => set("processName", v)}
						help={m.library_field_process_name_help()}
					/>
				</div>
			</Group>
			<Group title={m.library_field_audio()}>
				<div className="max-w-sm space-y-2">
					<Select
						value={draft.audioSessions}
						onValueChange={(v) => set("audioSessions", v as AudioSessions)}
					>
						<SelectTrigger
							id="entry-audio"
							size="sm"
							aria-label={m.library_field_audio()}
						>
							<SelectValue />
						</SelectTrigger>
						<SelectContent>
							<SelectItem value="all">{m.library_audio_all()}</SelectItem>
							<SelectItem value="owner">{m.library_audio_owner()}</SelectItem>
							<SelectItem value="joined">{m.library_audio_joined()}</SelectItem>
							<SelectItem value="launcher">
								{m.library_audio_launcher()}
							</SelectItem>
						</SelectContent>
					</Select>
					<p className="text-xs text-muted-foreground">
						{m.library_field_audio_help()}
					</p>
				</div>
			</Group>
			{(draft.prep?.length ?? 0) > 0 && (
				<Group
					title={m.library_entry_prep_title()}
					help={m.library_entry_prep_help()}
				>
					<ul className="space-y-2">
						{draft.prep?.map((cmd, i) => (
							<li
								key={i}
								className="rounded-md border bg-muted/40 px-3 py-2 font-mono text-xs"
							>
								{cmd.do}
								{cmd.undo && (
									<span className="block text-muted-foreground">
										↩ {cmd.undo}
									</span>
								)}
							</li>
						))}
					</ul>
				</Group>
			)}
		</>
	);
};
