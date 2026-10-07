import Section from "@unom/ui/section";
import {
	KeyRound,
	MonitorPlay,
	RefreshCw,
	Smartphone,
	Video,
	Volume2,
	ZapOff,
} from "lucide-react";
import type { FC, ReactNode } from "react";
import type { ActiveGame } from "@/api/gen/model/activeGame";
import type { AudioWiring } from "@/api/gen/model/audioWiring";
import type { GameEntry } from "@/api/gen/model/gameEntry";
import type { ProfilePublic } from "@/api/gen/model/profilePublic";
import type { RuntimeStatus } from "@/api/gen/model/runtimeStatus";
import type { SessionRow } from "@/api/gen/model/sessionRow";
import { QueryState } from "@/components/query-state";
import { Stagger } from "@/components/stagger";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { fmtNumber } from "@/lib/format";
import type { Loadable } from "@/lib/query";
import { m } from "@/paraglide/messages";
import { ActivityCard } from "@/sections/Activity";
import { LastSessionCard } from "./LastSessionCard";
import { RunningGames } from "./RunningGames";
import { SessionList } from "./SessionList";

export const DashboardView: FC<{
	status: Loadable<RuntimeStatus>;
	library?: GameEntry[];
	/** The box's profiles; with more than one, a session row names its player. */
	profiles?: ProfilePublic[];
	/** Host health warnings — renders nothing when the host is healthy (see `AttentionCard.tsx`).
	 * Sits above the status query on purpose: a host whose `/status` is failing is exactly when
	 * its health checks are worth reading. */
	attention?: ReactNode;
	onStopSession: () => void;
	onRequestIdr: () => void;
	onEndGame: (game: ActiveGame) => void;
	/** Per-session, by id — the host-wide `onStopSession` above stops every one of them. */
	onStopOne: (row: SessionRow) => void;
	onIdrOne: (row: SessionRow) => void;
	onMuteOne: (row: SessionRow, muted: boolean) => void;
	onAccessOne: (row: SessionRow, level: string) => void;
	/** Player slot for one session; `null` is the host's first-free claim. */
	onPlayerOne: (row: SessionRow, slot: number | null) => void;
	isStopping: boolean;
	isRequestingIdr: boolean;
	isEndingGame: boolean;
	isChangingSession: boolean;
}> = ({
	status,
	library,
	profiles,
	attention,
	onStopSession,
	onRequestIdr,
	onEndGame,
	onStopOne,
	onIdrOne,
	onMuteOne,
	onAccessOne,
	onPlayerOne,
	isStopping,
	isRequestingIdr,
	isEndingGame,
	isChangingSession,
}) => {
	const s = status.data;
	return (
		<Section maxWidth={false}>
			<div className="flex flex-col gap-card">
				<h1 className="text-2xl font-semibold">{m.status_title()}</h1>
				{attention}
				<QueryState
					isLoading={status.isLoading}
					error={status.error}
					refetch={status.refetch}
				>
					{/* A Stagger, not a div: these cards mount once /status answers, after the page animated. */}
					{s && (
						<Stagger className="flex flex-col gap-card">
							<div className="grid gap-card sm:grid-cols-2 lg:grid-cols-4">
								<StatCard
									icon={<Video className="size-4" />}
									label={m.status_video()}
									on={s.video_streaming}
								/>
								<StatCard
									icon={<Volume2 className="size-4" />}
									label={m.status_audio()}
									on={s.audio_streaming}
								/>
								{/* Both planes. GameStream and native (punktfunk/1) devices pair
								    into SEPARATE stores, and native is the DEFAULT one — counting
								    only the GameStream certs read as "0 paired" on a host every
								    one of whose clients was in fact paired. */}
								<Card>
									<CardContent className="flex flex-1 items-center justify-between">
										<span className="flex items-center gap-2 text-sm text-muted-foreground">
											<Smartphone className="size-4" />
											{m.status_paired_count()}
										</span>
										<span className="text-2xl font-semibold tabular-nums">
											{s.paired_clients + s.native_paired_clients}
										</span>
									</CardContent>
								</Card>
								<Card>
									<CardContent className="flex flex-1 items-center justify-between">
										<span className="flex items-center gap-2 text-sm text-muted-foreground">
											<KeyRound className="size-4" />
											{m.status_pin_pending()}
										</span>
										{/* The whole value used to be "●" or "—": no text, no state, colour
										    doing all the work — nothing for a screen reader to read out and
										    nothing for anyone who can't tell the two badges apart. */}
										<Badge variant={s.pin_pending ? "default" : "outline"}>
											{s.pin_pending
												? m.status_pin_waiting()
												: m.status_pin_none()}
										</Badge>
									</CardContent>
								</Card>
							</div>

							{/* The wiring verdict (Windows hosts): WHICH endpoints carry game
							    audio and the microphone, and the degradations that used to be
							    visible only in the host log — a silent host looks identical to a
							    quiet game without this. */}
							{s.audio && <AudioWiringCard audio={s.audio} />}

							{/* Above the session card: a game the host is about to close is the most
							    time-sensitive thing on this page. */}
							<RunningGames
								games={s.games}
								library={library}
								onEnd={onEndGame}
								isEnding={isEndingGame}
							/>

							{/* Above the stream card: the list is who is connected, the card below is
							    the numbers for one of them. */}
							<SessionList
								sessions={s.sessions}
								onStop={onStopOne}
								onIdr={onIdrOne}
								onMute={onMuteOne}
								onAccess={onAccessOne}
								onPlayer={onPlayerOne}
								busy={isChangingSession}
								profiles={profiles}
							/>

							<Card>
								<CardHeader className="flex flex-col items-start gap-3 space-y-0 sm:flex-row sm:items-center sm:justify-between">
									<CardTitle className="flex items-center gap-2">
										<MonitorPlay className="size-4" />
										{m.status_session()}
										{/* Which of the rows above these numbers belong to: the card is
										    singular and the list is not. */}
										{s.session_id != null && (
											<span className="text-xs font-normal text-muted-foreground tabular-nums">
												#{s.session_id}
											</span>
										)}
										{s.active_sessions > 1 && (
											<Badge variant="secondary">
												{m.status_sessions_active({ count: s.active_sessions })}
											</Badge>
										)}
									</CardTitle>
									<div className="flex flex-wrap gap-2">
										<Button
											variant="outline"
											size="sm"
											disabled={!s.video_streaming || isRequestingIdr}
											onClick={onRequestIdr}
										>
											<RefreshCw className="size-3.5" />
											{m.action_request_idr()}
										</Button>
										<Button
											variant={s.session ? "destructive" : "secondary"}
											size="sm"
											disabled={!s.session || isStopping}
											onClick={onStopSession}
										>
											<ZapOff className="size-3.5" />
											{m.action_stop_session()}
										</Button>
									</div>
								</CardHeader>
								<CardContent>
									{s.stream ? (
										<dl className="grid grid-cols-2 gap-x-6 gap-y-3 sm:grid-cols-4">
											<Field
												label={m.stream_codec()}
												value={s.stream.codec.toUpperCase()}
											/>
											<Field
												label={m.stream_resolution()}
												value={`${s.stream.width}×${s.stream.height}`}
											/>
											<Field
												label={m.stream_fps()}
												value={`${s.stream.fps} fps`}
											/>
											<Field
												label={m.stream_bitrate()}
												value={`${fmtNumber(s.stream.bitrate_kbps / 1000, 1)} Mbps`}
											/>
											{/* Bring-up and reconfigure cost, the parity floor and the packet
											    size: the host has reported all four for as long as this
											    endpoint has existed and the console showed none of them, so
											    "it takes ages to start" and "it hitches when I resize" had no
											    number attached anywhere. Native-plane only — null on
											    GameStream and null until the first frame lands, so the two
											    timings appear only once they mean something. */}
											{s.stream.time_to_first_frame_ms != null && (
												<Field
													label={m.stream_first_frame()}
													value={`${fmtNumber(s.stream.time_to_first_frame_ms)} ms`}
												/>
											)}
											{s.stream.last_resize_ms != null && (
												<Field
													label={m.stream_last_resize()}
													value={`${fmtNumber(s.stream.last_resize_ms)} ms`}
												/>
											)}
											{/* Windows IDD-push only: the capturer's live verdict and its last
											    staged-recovery episode, so a freeze is read here instead of
											    grepped out of the logs. */}
											{s.session?.capture != null && (
												<Field
													label={m.stream_capture_health()}
													value={
														s.session.capture.class +
														(s.session.capture.stall_class
															? ` (${s.session.capture.stall_class})`
															: "") +
														(s.session.capture.backend_opened
															? ` · ${s.session.capture.backend_opened}`
															: "")
													}
												/>
											)}
											{s.session?.capture?.last_episode != null && (
												<Field
													label={m.stream_last_recovery()}
													value={`${s.session.capture.last_episode.stall_class}: ${
														s.session.capture.last_episode.recovered
															? "recovered"
															: "failed"
													} after ${s.session.capture.last_episode.stages
														.map((st) => `${st.stage}=${st.outcome}`)
														.join(", ")} (${fmtNumber(
														s.session.capture.last_episode.took_ms / 1000,
														1,
													)} s)`}
												/>
											)}
											<Field
												label={m.stream_packet_size()}
												value={`${fmtNumber(s.stream.packet_size)} B`}
											/>
											<Field
												label={m.stream_min_fec()}
												value={fmtNumber(s.stream.min_fec)}
											/>
										</dl>
									) : (
										<p className="text-sm text-muted-foreground">
											{m.status_no_session()}
										</p>
									)}
								</CardContent>
							</Card>

							{/* Below the session card: the past, under the present. The summary
							    first — it is about the session the page was just showing. */}
							<LastSessionCard />
							<ActivityCard />
						</Stagger>
					)}
				</QueryState>
			</div>
		</Section>
	);
};

/**
 * One line per role plus a readiness badge; the degradation notes are spelled out because the
 * failure they describe (silent audio, a mic that quietly vanished) is invisible everywhere
 * else except the host log.
 */
const AudioWiringCard: FC<{ audio: AudioWiring }> = ({ audio }) => {
	const badge: {
		variant: "success" | "secondary" | "destructive";
		text: string;
	} =
		audio.readiness === "full"
			? { variant: "success", text: m.audio_ready() }
			: audio.readiness === "audio_only"
				? { variant: "secondary", text: m.audio_ready_no_mic() }
				: audio.readiness === "mic_only"
					? { variant: "destructive", text: m.audio_no_output() }
					: { variant: "destructive", text: m.audio_none() };
	const notes = [
		audio.mic_withheld ? m.audio_mic_withheld() : undefined,
		audio.last_resort ? m.audio_last_resort() : undefined,
		audio.narrowing,
	].filter((n): n is string => !!n);
	return (
		<Card>
			<CardHeader className="flex flex-row items-center justify-between space-y-0">
				<CardTitle className="flex items-center gap-2">
					<Volume2 className="size-4" />
					{m.audio_wiring_title()}
				</CardTitle>
				<Badge variant={badge.variant}>{badge.text}</Badge>
			</CardHeader>
			<CardContent className="flex flex-col gap-3">
				<dl className="grid gap-4 sm:grid-cols-2">
					<Field
						label={m.audio_output()}
						value={audio.loopback ?? m.audio_unavailable()}
					/>
					<Field
						label={m.audio_microphone()}
						value={audio.mic ?? m.audio_unavailable()}
					/>
				</dl>
				{notes.length > 0 && (
					<ul className="flex flex-col gap-1 text-sm text-muted-foreground">
						{notes.map((n) => (
							<li key={n}>{n}</li>
						))}
					</ul>
				)}
			</CardContent>
		</Card>
	);
};

const StatCard: FC<{ icon: ReactNode; label: string; on: boolean }> = ({
	icon,
	label,
	on,
}) => (
	<Card>
		<CardContent className="flex flex-1 items-center justify-between">
			<span className="flex items-center gap-2 text-sm text-muted-foreground">
				{icon}
				{label}
			</span>
			<Badge variant={on ? "success" : "outline"}>
				{on ? m.status_streaming() : m.status_idle()}
			</Badge>
		</CardContent>
	</Card>
);

const Field: FC<{ label: string; value: string }> = ({ label, value }) => (
	<div>
		<dt className="text-xs text-muted-foreground">{label}</dt>
		<dd className="mt-0.5 font-medium tabular-nums">{value}</dd>
	</div>
);
