import {
	QueryClientProvider,
	useMutation,
	useQueries,
	useQueryClient,
} from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import type { FC } from "react";
import {
	getGetStatusQueryKey,
	useGetHostInfo,
	useGetStatus,
} from "@/api/gen/host/host";
import {
	getGetLibraryPageQueryKey,
	getLibraryPage,
} from "@/api/gen/library/library";
import type { ActiveGame } from "@/api/gen/model/activeGame";
import type { ProfileAdmin } from "@/api/gen/model/profileAdmin";
import type { RuntimeStatus } from "@/api/gen/model/runtimeStatus";
import type { SessionRow } from "@/api/gen/model/sessionRow";
import {
	endProfileSession,
	getListProfilesQueryKey,
	stopProfileSeat,
	useListProfiles,
} from "@/api/gen/profiles/profiles";
import {
	useEndGame,
	useGetRecentSessions,
	useRequestIdr,
	useRequestSessionIdr,
	useSetSessionAccess,
	useSetSessionAudio,
	useSetSessionPlayer,
	useStopOneSession,
	useStopSession,
} from "@/api/gen/session/session";
import { isFullSeat, seatClient } from "@/api/seat";
import { useDialogs } from "@/components/dialogs";
import type { AvatarProfile } from "@/components/profile-avatar";
import { apiErrorMessage } from "@/lib/errors";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";
import { ActivityCard } from "@/sections/Activity";
import { Attention } from "./Attention";
import {
	GameRowView,
	LastLine,
	SeatRowView,
	type SessionActions,
	SessionRowView,
} from "./NowRows";
import { HomeView } from "./view";

const failed = (fallback: string) => (e: unknown) =>
	toast.error(apiErrorMessage(e) ?? fallback);

/**
 * The per-session verbs, against whichever host the surrounding query client reaches: the box's,
 * or a seat's through the box's proxy (`seatClient`).
 *
 * A row without an id is the compat plane, whose only handles are host-wide: its Stop stops every
 * session there, and says so first when more than one is live.
 */
function useSessionActions(active: number): SessionActions {
	const qc = useQueryClient();
	const { confirm } = useDialogs();
	const stopAll = useStopSession();
	const idrAll = useRequestIdr();
	const stopOne = useStopOneSession();
	const idrOne = useRequestSessionIdr();
	const mute = useSetSessionAudio();
	const access = useSetSessionAccess();
	const player = useSetSessionPlayer();
	const invalidate = () =>
		qc.invalidateQueries({ queryKey: getGetStatusQueryKey() });
	return {
		onStop: async (row) => {
			if (row.id != null)
				return stopOne.mutate(
					{ id: row.id },
					{ onSuccess: invalidate, onError: failed(m.action_stop_failed()) },
				);
			if (
				active > 1 &&
				!(await confirm({
					title: m.action_stop_session_all_title(),
					description: m.action_stop_session_all_confirm({ count: active }),
					confirmLabel: m.action_stop_session_all(),
					destructive: true,
				}))
			)
				return;
			stopAll.mutate(undefined, {
				onSuccess: invalidate,
				onError: failed(m.action_stop_failed()),
			});
		},
		onIdr: (row) =>
			row.id != null
				? idrOne.mutate(
						{ id: row.id },
						{ onError: failed(m.action_idr_failed()) },
					)
				: idrAll.mutate(undefined, { onError: failed(m.action_idr_failed()) }),
		onMute: (row, muted) =>
			row.id != null &&
			mute.mutate(
				{ id: row.id, data: { muted } },
				{ onSuccess: invalidate, onError: failed(m.action_mute_failed()) },
			),
		onAccess: (row, level) =>
			row.id != null &&
			access.mutate(
				{ id: row.id, data: { level } },
				{ onSuccess: invalidate, onError: failed(m.access_edit_failed()) },
			),
		onPlayer: (row, slot) =>
			row.id != null &&
			player.mutate(
				// `undefined`, not null: the host reads an omitted slot as the first-free claim.
				{ id: row.id, data: { slot: slot ?? undefined } },
				{ onSuccess: invalidate, onError: failed(m.action_player_failed()) },
			),
		busy:
			stopAll.isPending ||
			idrAll.isPending ||
			stopOne.isPending ||
			idrOne.isPending ||
			mute.isPending ||
			access.isPending ||
			player.isPending,
	};
}

/** The row the host's `stream`/`session` numbers describe. */
const representative = (s: RuntimeStatus, row: SessionRow) =>
	s.session_id != null ? row.id === s.session_id : row.plane === "gamestream";

/** The title a session streams, if the host launched one for it. A waiting game streams nowhere. */
const gameOf = (s: RuntimeStatus, row: SessionRow) =>
	s.games.find(
		(g) =>
			g.state !== "grace" &&
			g.state !== "detached" &&
			(g.session_id != null
				? g.session_id === row.id
				: row.id == null && g.plane === row.plane),
	);

/** Every session of one host as rows. */
const SessionRows: FC<{
	status: RuntimeStatus;
	actions: SessionActions;
	profileOf: (row: SessionRow) => AvatarProfile | undefined;
	seat?: boolean;
	end?: { label: string; onEnd: () => void };
}> = ({ status: s, actions, profileOf, seat, end }) => (
	<>
		{s.sessions.map((row, i) => {
			const lead = representative(s, row);
			return (
				<SessionRowView
					key={`${row.plane}:${row.id ?? "compat"}:${i}`}
					row={row}
					profile={profileOf(row)}
					game={gameOf(s, row)}
					stream={lead ? s.stream : undefined}
					info={lead ? s.session : undefined}
					sharedWith={(row.shared_path_with ?? []).map((id) => {
						const other = s.sessions.find((o) => o.id === id);
						return other?.client_name || other?.client || `#${id}`;
					})}
					seat={seat}
					end={end}
					{...actions}
				/>
			);
		})}
	</>
);

/**
 * An occupied seat's sessions, read from its own host through the box. A seat whose host does not
 * answer still gets its row: the profile, the occupant, and End.
 */
const SeatSessions: FC<{
	profile: ProfileAdmin;
	named: boolean;
	end: { label: string; onEnd: () => void; busy: boolean };
}> = ({ profile, named, end }) => {
	const status = useGetStatus({
		query: { refetchInterval: 2_000, retry: false },
	});
	const actions = useSessionActions(status.data?.active_sessions ?? 0);
	const s = status.data;
	if (!s || s.sessions.length === 0)
		return (
			<SeatRowView
				profile={profile}
				occupant={profile.seat?.occupant}
				facts={named ? m.home_own_desktop() : ""}
				action={{ label: end.label, onClick: end.onEnd, busy: end.busy }}
			/>
		);
	return (
		<SessionRows
			status={s}
			actions={actions}
			profileOf={() => profile}
			seat={named}
			end={end}
		/>
	);
};

/** Home: what needs attention, every session on every desktop, and what happened lately. */
export const SectionHome: FC = () => {
	useLocale();
	const qc = useQueryClient();
	const { confirm } = useDialogs();
	// Transitions arrive on the event stream; the timer covers the live numbers while streaming.
	const status = useGetStatus({
		query: {
			refetchInterval: (q) =>
				q.state.data?.video_streaming || (q.state.data?.games?.length ?? 0) > 0
					? 2_000
					: 15_000,
		},
	});
	const host = useGetHostInfo();
	// A host without profiles answers 404: no names, no seats.
	const profiles = useListProfiles({
		query: {
			retry: false,
			refetchInterval: (q) =>
				q.state.data?.some((p) => p.seat?.state === "starting")
					? 2_000
					: 15_000,
		},
	});
	const recent = useGetRecentSessions({ query: { retry: false } });
	const actions = useSessionActions(status.data?.active_sessions ?? 0);
	const endGame = useEndGame();
	const stop = useStopSession();
	const seatAct = useMutation({
		mutationFn: ({ id, act }: { id: string; act: "end" | "stop" }) =>
			act === "end" ? endProfileSession(id) : stopProfileSeat(id),
		onSuccess: (row) =>
			qc.setQueryData<ProfileAdmin[]>(getListProfilesQueryKey(), (rows) =>
				rows?.map((p) => (p.id === row.id ? row : p)),
			),
		onError: failed(m.profiles_seat_failed()),
	});

	// Box art for the games left without a stream. One title each: a library runs to thousands.
	const appIds = [
		...new Set(
			(status.data?.games ?? []).flatMap((g) => (g.app_id ? [g.app_id] : [])),
		),
	].sort();
	const covers = useQueries({
		queries: appIds.map((id) => ({
			queryKey: getGetLibraryPageQueryKey({ id, limit: 1 }),
			queryFn: ({ signal }: { signal: AbortSignal }) =>
				getLibraryPage({ id, limit: 1 }, { signal }),
			staleTime: 5 * 60_000,
		})),
	});
	const library = covers.flatMap((r) => r.data?.items ?? []);
	const coverOf = (g: ActiveGame) => {
		const e = g.app_id ? library.find((x) => x.id === g.app_id) : undefined;
		return e?.art.portrait ?? e?.art.header ?? undefined;
	};

	const list = profiles.data ?? [];
	// One person, no seat words (R9).
	const named = list.length > 1;
	const profileOf = (row: SessionRow) =>
		named && row.profile
			? (list.find((p) => p.id === row.profile?.id) ?? row.profile)
			: undefined;
	const door = host.data?.door === true;
	const seats = list.filter(
		(p) =>
			isFullSeat(p, host.data?.os, door) &&
			(p.seat?.state === "occupied" || p.seat?.state === "starting"),
	);

	/**
	 * A game nobody streams ends directly. `app_id: null` means "every unstreamed game" to the host,
	 * so a row without an id says so first when there are several.
	 */
	const onEndGame = async (game: ActiveGame) => {
		const games = status.data?.games ?? [];
		const unstreamed = (g: ActiveGame) =>
			g.state === "grace" || g.state === "detached";
		if (!unstreamed(game)) {
			if (game.session_id != null) {
				const row = status.data?.sessions.find((r) => r.id === game.session_id);
				if (row) actions.onStop(row);
				return;
			}
			stop.mutate(undefined, { onError: failed(m.action_stop_failed()) });
			return;
		}
		const waiting = games.filter(unstreamed).length;
		if (
			!game.app_id &&
			waiting > 1 &&
			!(await confirm({
				title: m.games_end_all_waiting_title({ count: waiting }),
				description: m.games_end_all_waiting_confirm({ count: waiting }),
				confirmLabel: m.games_end_now(),
				destructive: true,
			}))
		)
			return;
		endGame.mutate(
			{ data: { app_id: game.app_id ?? null } },
			{
				onSuccess: () =>
					qc.invalidateQueries({ queryKey: getGetStatusQueryKey() }),
				onError: failed(m.games_end_failed()),
			},
		);
	};

	const s = status.data;
	const streamed = (g: ActiveGame) =>
		s?.sessions.some((r) => gameOf(s, r) === g) ?? false;
	const orphans = (s?.games ?? []).filter((g) => !streamed(g));
	const live = (s?.sessions.length ?? 0) + seats.length + orphans.length > 0;

	return (
		<HomeView
			attention={<Attention audio={s?.audio} />}
			status={status}
			live={live}
			now={
				s && (
					<>
						<SessionRows status={s} actions={actions} profileOf={profileOf} />
						{seats.map((p) =>
							p.seat?.state === "starting" ? (
								<SeatRowView
									key={p.id}
									profile={p}
									starting
									facts={p.seat.detail || m.profiles_seat_starting()}
									action={{
										label: m.profiles_seat_stop(),
										onClick: () => seatAct.mutate({ id: p.id, act: "stop" }),
										busy: seatAct.isPending,
									}}
								/>
							) : (
								<QueryClientProvider key={p.id} client={seatClient(p.id, qc)}>
									<SeatSessions
										profile={p}
										named={named}
										end={{
											label: m.profiles_seat_end(),
											onEnd: () => seatAct.mutate({ id: p.id, act: "end" }),
											busy: seatAct.isPending,
										}}
									/>
								</QueryClientProvider>
							),
						)}
						{orphans.map((g, i) => (
							<GameRowView
								key={`${g.plane}:${g.state}:${g.app_id ?? g.title}:${i}`}
								game={g}
								art={coverOf(g)}
								onEnd={() => onEndGame(g)}
								isEnding={endGame.isPending || stop.isPending}
							/>
						))}
					</>
				)
			}
			last={<LastLine session={recent.data?.sessions?.[0]} />}
			recent={<ActivityCard />}
		/>
	);
};
