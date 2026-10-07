import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { type FC, type ReactNode, useEffect, useState } from "react";
import type {
	GameOnNewLaunch,
	GameOnSessionEnd,
	SessionSettings,
} from "@/api/gen/model";
import {
	getGetSessionSettingsQueryKey,
	useGetSessionSettings,
	useSetSessionSettings,
} from "@/api/gen/session/session";
import { usePlatform } from "@/api/platform";
import { Input } from "@/components/ui/input";
import { Segmented } from "@/components/ui/segmented";
import { apiErrorMessage } from "@/lib/errors";
import { m } from "@/paraglide/messages";

const END_POLICIES: GameOnSessionEnd[] = ["keep", "on_quit", "always"];
const NEW_LAUNCH_POLICIES: GameOnNewLaunch[] = ["keep", "end"];

const END_POLICY_LABEL: Record<GameOnSessionEnd, () => string> = {
	keep: () => m.session_game_end_keep(),
	on_quit: () => m.session_game_end_on_quit(),
	always: () => m.session_game_end_always(),
};

const NEW_LAUNCH_LABEL: Record<GameOnNewLaunch, () => string> = {
	keep: () => m.session_game_new_launch_keep(),
	end: () => m.session_game_new_launch_end(),
};

/**
 * Whether a launched game and its streaming session share a fate (design/session-game-lifetime.md),
 * as rows of Host → Session. They are session settings, not display ones: a kept display and a
 * kept game are separate decisions with separate timers. Every row saves on change.
 *
 * Their own plane (`/session/settings`), so `enforced` there gates each row.
 */
export const SessionGameRows: FC = () => {
	const qc = useQueryClient();
	const q = useGetSessionSettings();
	const save = useSetSessionSettings();
	const server = q.data?.settings;
	const { acts } = usePlatform();
	const enforces = (field: string) => acts("session", field);
	// Free text while typed; the host clamps to 10..=86400 on write and answers with what it stored.
	const [grace, setGrace] = useState("");
	useEffect(() => {
		if (server) setGrace(String(server.disconnect_grace_seconds ?? 300));
	}, [server]);

	if (!server) return null;
	const busy = save.isPending;
	const apply = (patch: Partial<SessionSettings>) =>
		save.mutate(
			{ data: { ...server, ...patch } },
			{
				onSuccess: () => {
					qc.invalidateQueries({ queryKey: getGetSessionSettingsQueryKey() });
					toast.success(m.session_game_saved());
				},
				onError: (e) =>
					toast.error(apiErrorMessage(e) ?? m.host_settings_save_failed()),
			},
		);
	const end = server.game_on_session_end ?? "keep";
	const launch = server.game_on_new_launch ?? "keep";

	return (
		<>
			{enforces("session_on_game_exit") && (
				<Row
					label={m.session_game_on_exit()}
					hint={m.session_game_on_exit_help()}
				>
					<Segmented
						busy={busy}
						value={server.session_on_game_exit}
						options={[
							[true, m.session_game_on_exit_end()],
							[false, m.session_game_on_exit_keep()],
						]}
						onPick={(v) => apply({ session_on_game_exit: v })}
					/>
				</Row>
			)}
			{enforces("game_on_session_end") && (
				<Row
					label={m.session_game_end_game()}
					hint={m.session_game_end_game_help()}
					// On a nested gamescope launch the game IS inside the streamed display, so the
					// display's own keep-alive outranks this; worded so other hosts read past it.
					notes={[
						end === "always" ? m.session_game_always_warning() : null,
						m.session_game_nested_note(),
					]}
				>
					<Segmented
						busy={busy}
						value={end}
						options={END_POLICIES.map(
							(p) => [p, END_POLICY_LABEL[p]()] as const,
						)}
						onPick={(p) => apply({ game_on_session_end: p })}
					/>
				</Row>
			)}
			{/* What a new launch owes the last game, not what a session owes its game. */}
			{enforces("game_on_new_launch") && (
				<Row
					label={m.session_game_new_launch()}
					hint={m.session_game_new_launch_help()}
					notes={[launch === "end" ? m.session_game_new_launch_scope() : null]}
				>
					<Segmented
						busy={busy}
						value={launch}
						options={NEW_LAUNCH_POLICIES.map(
							(p) => [p, NEW_LAUNCH_LABEL[p]()] as const,
						)}
						onPick={(p) => apply({ game_on_new_launch: p })}
					/>
				</Row>
			)}
			{enforces("disconnect_grace_seconds") && end === "always" && (
				<Row label={m.session_game_grace()} hint={m.session_game_grace_help()}>
					<span className="flex items-center gap-2">
						{/* Not `InputNumber`: that commits while typing, and this writes on blur. */}
						<Input
							aria-label={m.session_game_grace()}
							type="number"
							min={10}
							max={86400}
							className="w-28"
							value={grace}
							disabled={busy}
							onChange={(e) => setGrace(e.target.value)}
							onBlur={() => {
								const n = Number(grace);
								if (!Number.isFinite(n))
									return setGrace(
										String(server.disconnect_grace_seconds ?? 300),
									);
								if (n !== server.disconnect_grace_seconds)
									apply({ disconnect_grace_seconds: n });
							}}
						/>
						<span className="text-sm text-muted-foreground">
							{m.display_keep_alive_seconds()}
						</span>
					</span>
				</Row>
			)}
		</>
	);
};

/** The shape of a Host → Settings row: what it is on the left, its control on the right. */
const Row: FC<{
	label: string;
	hint: string;
	notes?: (string | null)[];
	children: ReactNode;
}> = ({ label, hint, notes = [], children }) => (
	<li className="flex flex-col gap-3 py-4 first:pt-1 last:pb-1 md:flex-row md:items-start md:justify-between md:gap-8">
		<div className="min-w-0 space-y-1 md:max-w-md">
			<span className="text-sm font-medium">{label}</span>
			<p className="text-xs text-muted-foreground">{hint}</p>
			{notes
				.filter((n): n is string => !!n)
				.map((n) => (
					<p key={n} className="text-xs text-muted-foreground">
						{n}
					</p>
				))}
		</div>
		<div className="md:shrink-0">{children}</div>
	</li>
);
