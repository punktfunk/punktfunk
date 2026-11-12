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
	useGetSessionSettings,
	useSetSessionSettings,
} from "@/api/gen/session/session";
import { seatClient } from "@/api/seat";
import { Segmented } from "@/components/ui/segmented";
import { Spinner } from "@/components/ui/spinner";
import { apiErrorMessage } from "@/lib/errors";
import { m } from "@/paraglide/messages";

/** `Segmented` value for following the host; the other two are the stored switch. */
const HOST = "host";
type Choice = typeof HOST | boolean;

/**
 * The card's **When the game exits** for `profile`. Behind a door the owner's host stores it, so
 * the call goes there; a person's own Linux account has it fixed, and says so.
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
	const state = q.data;
	return (
		<GameExitView
			state={state}
			loading={q.isLoading}
			error={q.error ? (apiErrorMessage(q.error) ?? m.common_error()) : null}
			busy={save.isPending}
			onPick={(end) => {
				if (!state) return;
				save.mutate(
					{
						data: { ...state.settings, session_on_game_exit: end },
						params: as,
					},
					{
						onSuccess: (next) => {
							qc.setQueryData(getGetSessionSettingsQueryKey(as), next);
							toast.success(m.session_game_saved());
						},
						onError: (e) =>
							toast.error(apiErrorMessage(e) ?? m.host_settings_save_failed()),
					},
				);
			}}
		/>
	);
};

/** The row: its label beside its control, indented under the profile's name as the signed-in
 * devices are, and a line below. */
const Frame: FC<{ children: ReactNode; below?: ReactNode }> = ({
	children,
	below,
}) => (
	<div className="space-y-1 sm:pl-14">
		<div className="flex flex-wrap items-center gap-x-4 gap-y-2">
			<h3 className="text-sm font-medium">{m.session_game_on_exit()}</h3>
			{children}
		</div>
		{below}
	</div>
);

/** The fixed behaviour on a person's own Linux account, as a sentence. */
export const GameExitFixed: FC<{ name: string }> = ({ name }) => (
	<p className="text-sm text-muted-foreground sm:pl-14">
		{m.profiles_game_exit_own_desktop({ name })}
	</p>
);

/**
 * **As the host does**, **End the session**, **Keep streaming**. Following the host is offered
 * while the profile has no setting of its own: the host has no way yet to drop one.
 */
export const GameExitView: FC<{
	state: SessionSettingsState | undefined;
	loading: boolean;
	/** The host's sentence when the setting couldn't be read. */
	error: string | null;
	busy: boolean;
	onPick: (end: boolean) => void;
}> = ({ state, loading, error, busy, onPick }) => {
	if (state && !state.enforced.includes("session_on_game_exit")) return null;
	const end = state?.settings.session_on_game_exit === true;
	const value: Choice | undefined = !state
		? undefined
		: state.configured
			? end
			: HOST;
	return (
		<Frame
			below={
				!loading &&
				!error &&
				value === HOST && (
					<p className="text-xs text-muted-foreground">
						{end
							? m.profiles_game_exit_host_end()
							: m.profiles_game_exit_host_keep()}
					</p>
				)
			}
		>
			{loading ? (
				<span
					role="status"
					className="flex items-center gap-2 text-sm text-muted-foreground"
				>
					<Spinner className="size-4" />
					{m.common_loading()}
				</span>
			) : error ? (
				<p role="alert" className="text-sm text-destructive">
					{error}
				</p>
			) : (
				<Segmented<Choice>
					busy={busy}
					label={m.session_game_on_exit()}
					value={value}
					options={[
						...(state?.configured
							? []
							: [[HOST, m.profiles_game_exit_host()] as const]),
						[true, m.session_game_on_exit_end()],
						[false, m.session_game_on_exit_keep()],
					]}
					onPick={(v) => v !== HOST && onPick(v)}
				/>
			)}
		</Frame>
	);
};
