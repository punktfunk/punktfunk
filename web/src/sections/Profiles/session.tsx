// What a game's exit does to the stream of a profile whose seat has a host of its own. It is the
// profile's own session setting, which the box stores and that seat's host plays with; absent,
// the seat follows the host's choice for every seat.
import { QueryClientProvider, useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import type { FC, ReactNode } from "react";
import type { ProfileAdmin } from "@/api/gen/model/profileAdmin";
import type { SessionSettingsState } from "@/api/gen/model/sessionSettingsState";
import {
	getGetSessionSettingsQueryKey,
	useClearSessionSettings,
	useGetSessionSettings,
	useSetSessionSettings,
} from "@/api/gen/session/session";
import { seatClient } from "@/api/seat";
import { Button } from "@/components/ui/button";
import { Segmented } from "@/components/ui/segmented";
import { Spinner } from "@/components/ui/spinner";
import { apiErrorMessage } from "@/lib/errors";
import { m } from "@/paraglide/messages";

/** `Segmented` value for following the host; the other two are the stored switch. */
const HOST = "host";
export type GameExitChoice = typeof HOST | boolean;

/**
 * **When the game exits** for `profile`, in its edit dialog. Behind a door the owner's host
 * stores it, so the call goes there; a person's own Linux account has it fixed, and says so.
 */
export const GameExitPreference: FC<{
	profile: ProfileAdmin;
	/** The owner's profile id on a door, whose host answers; absent off a door. */
	via?: string;
	/** A person's own Linux account: a game's exit always goes back to their desktop. */
	fixed?: boolean;
}> = ({ profile, via, fixed }) => {
	const root = useQueryClient();
	if (fixed) return <GameExitFixed name={profile.display_name} />;
	const body = <GameExitQuery id={profile.id} />;
	return via ? (
		<QueryClientProvider client={seatClient(via, root)}>
			{body}
		</QueryClientProvider>
	) : (
		body
	);
};

const GameExitQuery: FC<{ id: string }> = ({ id }) => {
	const qc = useQueryClient();
	const as = { as: id };
	const q = useGetSessionSettings(as);
	const save = useSetSessionSettings();
	// Drops the profile's own setting; the host answers what applies then, the host's.
	const drop = useClearSessionSettings();
	const state = q.data;
	const done = {
		onSuccess: (next: SessionSettingsState) => {
			qc.setQueryData(getGetSessionSettingsQueryKey(as), next);
			toast.success(m.session_game_saved());
		},
		onError: (e: unknown) =>
			toast.error(apiErrorMessage(e) ?? m.host_settings_save_failed()),
	};
	return (
		<GameExitView
			state={state}
			loading={q.isLoading}
			error={q.error ? (apiErrorMessage(q.error) ?? m.common_error()) : null}
			onRetry={() => void q.refetch()}
			busy={save.isPending || drop.isPending}
			onPick={(choice) => {
				if (!state) return;
				if (choice !== HOST)
					save.mutate(
						{
							data: { ...state.settings, session_on_game_exit: choice },
							params: as,
						},
						done,
					);
				else if (state.configured) drop.mutate({ params: as }, done);
			}}
		/>
	);
};

/** The dialog's part: its label, then the control and a line below. */
const Frame: FC<{ children: ReactNode }> = ({ children }) => (
	<div className="space-y-2">
		<h3 className="text-sm font-medium">{m.session_game_on_exit()}</h3>
		{children}
	</div>
);

/** The fixed behaviour on a person's own Linux account, as a sentence. */
export const GameExitFixed: FC<{ name: string }> = ({ name }) => (
	<Frame>
		<p className="text-sm text-muted-foreground">
			{m.profiles_game_exit_own_desktop({ name })}
		</p>
	</Frame>
);

/**
 * **As the host does**, **End the session**, **Keep streaming**, and while it follows the host,
 * what the host does. Loading and a failure stay inside the part.
 */
export const GameExitView: FC<{
	state: SessionSettingsState | undefined;
	loading: boolean;
	/** The host's sentence when the setting couldn't be read. */
	error: string | null;
	onRetry?: () => void;
	busy: boolean;
	onPick: (choice: GameExitChoice) => void;
}> = ({ state, loading, error, onRetry, busy, onPick }) => {
	if (state && !state.enforced.includes("session_on_game_exit")) return null;
	const end = state?.settings.session_on_game_exit === true;
	const value: GameExitChoice | undefined = !state
		? undefined
		: state.configured
			? end
			: HOST;
	return (
		<Frame>
			{loading ? (
				<span
					role="status"
					className="flex items-center gap-2 text-sm text-muted-foreground"
				>
					<Spinner className="size-4" />
					{m.common_loading()}
				</span>
			) : error ? (
				<div role="alert" className="space-y-2">
					<p className="text-sm text-destructive">{error}</p>
					{onRetry && (
						<Button type="button" variant="outline" size="sm" onClick={onRetry}>
							{m.common_retry()}
						</Button>
					)}
				</div>
			) : (
				<>
					<Segmented<GameExitChoice>
						busy={busy}
						label={m.session_game_on_exit()}
						value={value}
						options={[
							[HOST, m.profiles_game_exit_host()],
							[true, m.session_game_on_exit_end()],
							[false, m.session_game_on_exit_keep()],
						]}
						onPick={onPick}
					/>
					{value === HOST && (
						<p className="text-xs text-muted-foreground">
							{end
								? m.profiles_game_exit_host_end()
								: m.profiles_game_exit_host_keep()}
						</p>
					)}
				</>
			)}
		</Frame>
	);
};
