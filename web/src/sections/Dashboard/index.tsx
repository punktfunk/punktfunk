import { useQueries, useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import type { FC } from "react";
import { getGetStatusQueryKey, useGetStatus } from "@/api/gen/host/host";
import {
	getGetLibraryPageQueryKey,
	getLibraryPage,
} from "@/api/gen/library/library";
import type { ActiveGame } from "@/api/gen/model/activeGame";
import { useListProfiles } from "@/api/gen/profiles/profiles";
import {
	useEndGame,
	useRequestIdr,
	useRequestSessionIdr,
	useSetSessionAccess,
	useSetSessionAudio,
	useSetSessionPlayer,
	useStopOneSession,
	useStopSession,
} from "@/api/gen/session/session";
import { useDialogs } from "@/components/dialogs";
import { apiErrorMessage } from "@/lib/errors";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";
import { AttentionCard } from "./AttentionCard";
import { DashboardView } from "./view";

export const SectionDashboard: FC = () => {
	useLocale();
	const qc = useQueryClient();
	const { confirm } = useDialogs();
	// Session/game transitions arrive on the event stream now (api/events.ts invalidates this key),
	// so the timer only has to cover what events cannot: the live stream numbers — codec, resolution,
	// fps, bitrate — which change continuously while something is streaming. Idle, it is a slow
	// safety net in case the stream is unavailable.
	const status = useGetStatus({
		query: {
			refetchInterval: (q) =>
				q.state.data?.video_streaming || (q.state.data?.games?.length ?? 0) > 0
					? 2_000
					: 15_000,
		},
	});
	// The running games' own entries, for their box art. One title each and held: a library can
	// run to thousands, and nothing here needs the rest of it.
	const appIds = [
		...new Set(
			(status.data?.games ?? []).flatMap((g) => (g.app_id ? [g.app_id] : [])),
		),
	].sort();
	const running = useQueries({
		queries: appIds.map((id) => ({
			queryKey: getGetLibraryPageQueryKey({ id, limit: 1 }),
			queryFn: ({ signal }: { signal: AbortSignal }) =>
				getLibraryPage({ id, limit: 1 }, { signal }),
			staleTime: 5 * 60_000,
		})),
	});
	const library = running.flatMap((r) => r.data?.items ?? []);
	// Names and pictures for the session rows. A host without profiles answers 404: no names.
	const profiles = useListProfiles({
		query: { staleTime: 60_000, retry: false },
	});
	const stop = useStopSession();
	const idr = useRequestIdr();
	const endGame = useEndGame();
	// The per-session verbs. Same actions, one id — the host-wide ones above still mean
	// "every session", which is what every existing caller expects of them.
	const stopOne = useStopOneSession();
	const idrOne = useRequestSessionIdr();
	const mute = useSetSessionAudio();
	const access = useSetSessionAccess();
	const player = useSetSessionPlayer();

	const invalidate = () =>
		qc.invalidateQueries({ queryKey: getGetStatusQueryKey() });

	/** Every session control reports its failure. These are the console's most consequential
	 * buttons — stopping a session, ending a game — and a refusal used to be completely silent. */
	const failed = (fallback: string) => (e: unknown) =>
		toast.error(apiErrorMessage(e) ?? fallback);

	/**
	 * "End now" means two different things, and which one is right follows from the row's state: a
	 * game whose session is still live ends by stopping that session (what then happens to the game
	 * follows the operator's policy — stopping a session is not licence to close a game), while a
	 * game nobody is streaming (`grace` or `detached`) has no session left to stop and is ended directly.
	 *
	 * A live native row stops its OWN session by id, so ending one person's game no longer kicks
	 * everyone else off. Two paths are still wider than the row, and both say so before acting:
	 *
	 * - a compat-plane row has no session id, and the host's only stop for it is `DELETE /session`,
	 *   which tears down every live session on both planes.
	 * - `POST /game/end` with `app_id: null` means "end EVERY game nobody is streaming" to the host,
	 *   and a grace row for an operator-typed command carries no `app_id` — so that row ends all of them.
	 */
	const onEndGame = async (game: ActiveGame) => {
		const games = status.data?.games ?? [];
		const unstreamed = (g: ActiveGame) =>
			g.state === "grace" || g.state === "detached";
		if (unstreamed(game)) {
			const waiting = games.filter(unstreamed).length;
			if (!game.app_id && waiting > 1) {
				const ok = await confirm({
					title: m.games_end_all_waiting_title({ count: waiting }),
					description: m.games_end_all_waiting_confirm({ count: waiting }),
					confirmLabel: m.games_end_now(),
					destructive: true,
				});
				if (!ok) return;
			}
			endGame.mutate(
				{ data: { app_id: game.app_id ?? null } },
				{ onSuccess: invalidate, onError: failed(m.games_end_failed()) },
			);
			return;
		}
		if (game.session_id != null) {
			stopOne.mutate(
				{ id: game.session_id },
				{ onSuccess: invalidate, onError: failed(m.action_stop_failed()) },
			);
			return;
		}
		if (!(await confirmStopAll())) return;
		stop.mutate(undefined, {
			onSuccess: invalidate,
			onError: failed(m.action_stop_failed()),
		});
	};

	/** Shared by "End now" on a live row and the card's own Stop-session button: with more than one
	 * session live, stopping is not a per-client action and the operator has to know that. */
	const confirmStopAll = (): Promise<boolean> => {
		const active = status.data?.active_sessions ?? 0;
		if (active <= 1) return Promise.resolve(true);
		return confirm({
			title: m.action_stop_session_all_title(),
			description: m.action_stop_session_all_confirm({ count: active }),
			confirmLabel: m.action_stop_session_all(),
			destructive: true,
		});
	};

	return (
		<DashboardView
			status={status}
			library={library}
			profiles={profiles.data}
			attention={<AttentionCard />}
			onStopSession={async () => {
				if (!(await confirmStopAll())) return;
				stop.mutate(undefined, {
					onSuccess: invalidate,
					onError: failed(m.action_stop_failed()),
				});
			}}
			onRequestIdr={() =>
				idr.mutate(undefined, { onError: failed(m.action_idr_failed()) })
			}
			onEndGame={onEndGame}
			onStopOne={(row) =>
				row.id != null &&
				stopOne.mutate(
					{ id: row.id },
					{ onSuccess: invalidate, onError: failed(m.action_stop_failed()) },
				)
			}
			onIdrOne={(row) =>
				row.id != null &&
				idrOne.mutate(
					{ id: row.id },
					{ onError: failed(m.action_idr_failed()) },
				)
			}
			onMuteOne={(row, muted) =>
				row.id != null &&
				mute.mutate(
					{ id: row.id, data: { muted } },
					{ onSuccess: invalidate, onError: failed(m.action_mute_failed()) },
				)
			}
			onAccessOne={(row, level) =>
				row.id != null &&
				access.mutate(
					{ id: row.id, data: { level } },
					{ onSuccess: invalidate, onError: failed(m.access_edit_failed()) },
				)
			}
			onPlayerOne={(row, slot) =>
				row.id != null &&
				player.mutate(
					// `undefined`, not null: the host reads an omitted slot as the
					// first-free claim, and the generated body has no nullable arm.
					{ id: row.id, data: { slot: slot ?? undefined } },
					{ onSuccess: invalidate, onError: failed(m.action_player_failed()) },
				)
			}
			isStopping={stop.isPending}
			isRequestingIdr={idr.isPending}
			isEndingGame={endGame.isPending || stop.isPending}
			isChangingSession={
				stopOne.isPending ||
				idrOne.isPending ||
				mute.isPending ||
				access.isPending ||
				player.isPending
			}
		/>
	);
};
