import {
	Check,
	Clipboard,
	Gamepad2,
	ImageUp,
	Loader2,
	MonitorPlay,
	Volume2,
	VolumeX,
	XCircle,
	ZapOff,
} from "lucide-react";
import { motion } from "motion/react";
import { type FC, type ReactNode, useState } from "react";
import type { ActiveGame } from "@/api/gen/model/activeGame";
import type { SessionInfo } from "@/api/gen/model/sessionInfo";
import type { SessionRow } from "@/api/gen/model/sessionRow";
import type { SessionSummary } from "@/api/gen/model/sessionSummary";
import type { StreamInfo } from "@/api/gen/model/streamInfo";
import { type AvatarProfile, ProfileAvatar } from "@/components/profile-avatar";
import { ROW } from "@/components/stagger";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { RowActions } from "@/components/ui/menu";
import {
	fmtAgo,
	fmtClockDuration,
	fmtLinkRate,
	fmtNumber,
	fmtSpan,
} from "@/lib/format";
import { m } from "@/paraglide/messages";
import { levelLabel } from "@/sections/Devices/access";

/**
 * Home's *Now*: one row per session on every desktop, a seat that is starting, and a game left
 * running without a stream. The shapes are shared by the box's rows and a seat's (`index.tsx`).
 */

/** A game's cover at the head of its row, or a pad where the library has none. */
const Cover: FC<{ art?: string }> = ({ art }) => (
	<div className="flex h-14 w-10 items-center justify-center overflow-hidden rounded-md bg-muted shadow-sm ring-1 ring-border">
		{art ? (
			<img src={art} alt="" loading="lazy" className="size-full object-cover" />
		) : (
			<Gamepad2 className="size-4 text-muted-foreground/60" />
		)}
	</div>
);

/** The row frame: who, what, the facts line, then its actions. Rows rise in one after another. */
const Frame: FC<{
	lead: ReactNode;
	title: ReactNode;
	facts: string;
	details?: ReactNode;
	actions: ReactNode;
}> = ({ lead, title, facts, details, actions }) => (
	<motion.li
		variants={ROW}
		className="flex items-center gap-3 py-3 first:pt-0 last:pb-0"
	>
		<div className="flex w-10 shrink-0 items-center justify-center">{lead}</div>
		<div className="min-w-0 flex-1">
			<div className="flex flex-wrap items-center gap-x-2 gap-y-1">{title}</div>
			<p className="mt-0.5 text-xs text-muted-foreground">{facts}</p>
			{details}
		</div>
		<div className="flex shrink-0 items-center gap-1">{actions}</div>
	</motion.li>
);

/** A row button's words: a phone keeps the icon and leaves the width to the row. */
const Label: FC<{ children: string }> = ({ children }) => (
	<>
		<span className="hidden sm:inline">{children}</span>
		<span className="sr-only sm:hidden">{children}</span>
	</>
);

/** Who plays: the profile when the box has several, else the device. */
const Who: FC<{ profile?: AvatarProfile; device: string }> = ({
	profile,
	device,
}) =>
	profile ? (
		<>
			<span className="truncate font-medium">{profile.display_name}</span>
			<span className="truncate text-muted-foreground">{device}</span>
		</>
	) : (
		<span className="truncate font-medium">{device}</span>
	);

const Field: FC<{ label: string; value: string }> = ({ label, value }) => (
	<div>
		<dt className="text-xs text-muted-foreground">{label}</dt>
		<dd className="mt-0.5 font-medium tabular-nums">{value}</dd>
	</div>
);

/** The numbers behind a row, folded: a fact is a line in a disclosure (R4). */
const Details: FC<{ children: ReactNode }> = ({ children }) => (
	<details className="group mt-1">
		<summary className="cursor-pointer text-xs text-muted-foreground hover:text-foreground">
			{m.common_details()}
		</summary>
		<div className="mt-2">{children}</div>
	</details>
);

/** The stream numbers the host reports for its representative session. */
function streamFields(stream: StreamInfo, info?: SessionInfo | null) {
	const capture = info?.capture;
	const episode = capture?.last_episode;
	return [
		[m.stream_codec(), stream.codec.toUpperCase()],
		[m.stream_resolution(), `${stream.width}×${stream.height}`],
		[m.stream_fps(), `${stream.fps} fps`],
		[m.stream_bitrate(), `${fmtNumber(stream.bitrate_kbps / 1000, 1)} Mbit/s`],
		stream.time_to_first_frame_ms != null && [
			m.stream_first_frame(),
			`${fmtNumber(stream.time_to_first_frame_ms)} ms`,
		],
		stream.last_resize_ms != null && [
			m.stream_last_resize(),
			`${fmtNumber(stream.last_resize_ms)} ms`,
		],
		capture != null && [
			m.stream_capture_health(),
			capture.class +
				(capture.stall_class ? ` (${capture.stall_class})` : "") +
				(capture.backend_opened ? ` · ${capture.backend_opened}` : ""),
		],
		episode != null && [
			m.stream_last_recovery(),
			`${episode.stall_class}: ${episode.recovered ? "recovered" : "failed"} after ${episode.stages
				.map((st) => `${st.stage}=${st.outcome}`)
				.join(", ")} (${fmtNumber(episode.took_ms / 1000, 1)} s)`,
		],
		[m.stream_packet_size(), `${fmtNumber(stream.packet_size)} B`],
		[m.stream_min_fec(), fmtNumber(stream.min_fec)],
	].filter((f): f is [string, string] => Array.isArray(f));
}

/** No pick: the slot is whichever comes free. Not a slot number, so it cannot collide with one. */
const AUTO_PLAYER = "auto";

export interface SessionActions {
	onStop: (row: SessionRow) => void;
	onIdr: (row: SessionRow) => void;
	onMute: (row: SessionRow, muted: boolean) => void;
	onAccess: (row: SessionRow, level: string) => void;
	/** `null` hands the session back to the host's first-free claim. */
	onPlayer: (row: SessionRow, slot: number | null) => void;
	busy: boolean;
}

/**
 * A live session. `stream` is set only on the row the host reports numbers for. A seat's row
 * says so (`seat`), and its primary action ends the seat's session (`endLabel`). Its folded
 * facts name the link rate every frame is paced at, the wake shape while it is on, and where
 * the last minute's lost shards fell in their frames (`link`, absent in the first minute).
 *
 * Mute and the player slot ride native-only lanes; access is ungoverned on the compat plane, so
 * none of the three is offered there.
 */
export const SessionRowView: FC<
	SessionActions & {
		row: SessionRow;
		profile?: AvatarProfile;
		game?: ActiveGame;
		stream?: StreamInfo | null;
		info?: SessionInfo | null;
		/** Names of the other sessions on this row's client address. */
		sharedWith: string[];
		seat?: boolean;
		/** The primary action, when it is not this session's Stop. */
		end?: { label: string; onEnd: () => void };
		/** The streamed game's cover, when the library has one. */
		art?: string;
	}
> = ({
	row,
	profile,
	game,
	stream,
	info,
	sharedWith,
	seat,
	end,
	art,
	onStop,
	onIdr,
	onMute,
	onAccess,
	onPlayer,
	busy,
}) => {
	const perSession = row.id != null;
	const nativeLanes = row.plane !== "gamestream";
	const link = row.link;
	const lost = link ? link.loss_head + link.loss_mid + link.loss_tail : 0;
	// With a game, the game is the title and the player drops to the facts line.
	const who = [profile?.display_name, row.client_name || row.client]
		.filter(Boolean)
		.join(" · ");
	const facts = [
		game ? who : undefined,
		seat ? m.home_own_desktop() : undefined,
		row.mode,
		stream
			? `${stream.codec.toUpperCase()} · ${fmtNumber(stream.bitrate_kbps / 1000, 1)} Mbit/s`
			: undefined,
		fmtSpan(row.uptime_s),
	].filter(Boolean);
	const more = [
		row.preset_name,
		row.join ? m.sessions_joined() : m.sessions_own_display(),
		sharedWith.length > 0
			? m.sessions_shared_path({ names: sharedWith.join(", ") })
			: undefined,
		row.plane === "gamestream" ? "GameStream" : undefined,
		row.plane === "web" ? m.sessions_plane_web() : undefined,
		link && link.link_kbps > 0
			? m.sessions_link({ rate: fmtLinkRate(link.link_kbps) })
			: undefined,
		link?.shape === "wake" ? m.sessions_link_wake() : undefined,
		link && lost > 0
			? m.sessions_link_lost({
					head: link.loss_head,
					mid: link.loss_mid,
					tail: link.loss_tail,
				})
			: undefined,
	].filter(Boolean);
	return (
		<Frame
			lead={
				game ? (
					<Cover art={art} />
				) : profile ? (
					<ProfileAvatar profile={profile} className="size-7 text-xs" />
				) : (
					<MonitorPlay className="size-4 text-muted-foreground" />
				)
			}
			title={
				<>
					<span className="size-2 rounded-full bg-[var(--success)]" />
					{game ? (
						<span className="truncate font-medium">{game.title}</span>
					) : (
						<Who profile={profile} device={row.client_name || row.client} />
					)}
					{row.muted && <Badge variant="secondary">{m.sessions_muted()}</Badge>}
					{row.pads.map((slot) => (
						<Badge key={slot} variant="outline" className="tabular-nums">
							<Gamepad2 className="size-3" />
							{m.sessions_player_n({ n: slot + 1 })}
						</Badge>
					))}
				</>
			}
			facts={facts.join(" · ")}
			details={
				<Details>
					<p className="text-xs text-muted-foreground">{more.join(" · ")}</p>
					{stream && (
						<dl className="mt-2 grid grid-cols-2 gap-x-6 gap-y-3 sm:grid-cols-4">
							{streamFields(stream, info).map(([label, value]) => (
								<Field key={label} label={label} value={value} />
							))}
						</dl>
					)}
				</Details>
			}
			actions={
				<>
					<RowActions
						disabled={!perSession}
						labelsFrom="xl"
						actions={[
							{
								label: m.action_request_idr(),
								icon: <ImageUp />,
								disabled: busy,
								onSelect: () => onIdr(row),
							},
							nativeLanes && {
								label: row.muted ? m.action_unmute() : m.action_mute(),
								icon: row.muted ? <Volume2 /> : <VolumeX />,
								disabled: busy,
								onSelect: () => onMute(row, !row.muted),
							},
							row.access_level && {
								kind: "choice",
								label: m.access_level_label(),
								value: row.access_level,
								disabled: busy,
								onChange: (level) => onAccess(row, level),
								options: [
									{ value: "full", label: m.access_level_full() },
									{ value: "controller", label: m.access_level_controller() },
									{ value: "view", label: m.access_level_view() },
									// Only while the live mask is one the three presets cannot name.
									...(row.access_level === "custom"
										? [
												{
													value: "custom",
													label: levelLabel("custom"),
													disabled: true,
												},
											]
										: []),
								],
							},
							// Four, not the host's sixteen slots: local co-op seats four.
							nativeLanes && {
								kind: "choice",
								label: m.sessions_player(),
								value: row.preferred_pad_slot?.toString() ?? AUTO_PLAYER,
								disabled: busy,
								onChange: (v) =>
									onPlayer(row, v === AUTO_PLAYER ? null : Number(v)),
								options: [
									{ value: AUTO_PLAYER, label: m.sessions_player_auto() },
									...[0, 1, 2, 3].map((slot) => ({
										value: slot.toString(),
										label: m.sessions_player_n({ n: slot + 1 }),
									})),
								],
							},
						]}
					/>
					<Button
						variant="destructive"
						size="sm"
						disabled={busy || (!end && !perSession)}
						onClick={end ? end.onEnd : () => onStop(row)}
					>
						<ZapOff className="size-3.5" />
						<Label>{end ? end.label : m.action_stop_session()}</Label>
					</Button>
				</>
			}
		/>
	);
};

/**
 * A game no listed session streams: waiting for its client (`grace`, on a countdown that costs
 * unsaved progress), left running (`detached`), or one the host can't track.
 */
export const GameRowView: FC<{
	game: ActiveGame;
	art?: string;
	onEnd: () => void;
	isEnding: boolean;
}> = ({ game, art, onEnd, isEnding }) => {
	const waiting = game.state === "grace";
	return (
		<Frame
			lead={<Cover art={art} />}
			title={
				<>
					<span className="truncate font-medium">{game.title}</span>
					<Badge variant={waiting ? "destructive" : stateVariant(game.state)}>
						{stateLabel(game.state)}
					</Badge>
				</>
			}
			facts={
				waiting
					? m.games_closing_in({
							time: fmtClockDuration(game.grace_remaining_s ?? 0),
						})
					: game.state === "untracked"
						? m.games_untracked_note()
						: game.state === "detached"
							? m.games_detached_note()
							: [game.client, game.plane === "gamestream" ? "GameStream" : ""]
									.filter(Boolean)
									.join(" · ")
			}
			actions={
				<Button
					variant={waiting ? "destructive" : "outline"}
					size="sm"
					disabled={isEnding}
					onClick={onEnd}
				>
					<XCircle className="size-3.5" />
					<Label>{m.games_end_now()}</Label>
				</Button>
			}
		/>
	);
};

/**
 * A seat with nothing to report but its own state: starting, or occupied while its host does not
 * answer the proxy. The row still names the profile, the device and what the seat says.
 */
export const SeatRowView: FC<{
	profile: AvatarProfile;
	occupant?: string | null;
	facts: string;
	starting?: boolean;
	action: { label: string; onClick: () => void; busy: boolean };
}> = ({ profile, occupant, facts, starting, action }) => (
	<Frame
		lead={<ProfileAvatar profile={profile} className="size-7 text-xs" />}
		title={
			<>
				{starting ? (
					<Loader2 className="size-3 animate-spin text-muted-foreground" />
				) : (
					<span className="size-2 rounded-full bg-[var(--success)]" />
				)}
				<Who profile={profile} device={occupant || "—"} />
			</>
		}
		facts={facts}
		actions={
			<Button
				variant={starting ? "outline" : "destructive"}
				size="sm"
				disabled={action.busy}
				onClick={action.onClick}
			>
				<ZapOff className="size-3.5" />
				<Label>{action.label}</Label>
			</Button>
		}
	/>
);

/** Why it ended, in the console's own words. Exhaustive: a reason the host adds fails the build. */
function endedLabel(s: SessionSummary): string {
	switch (s.ended) {
		case "local":
			return m.status_last_end_local();
		case "game_exited":
			return m.status_last_end_game_exited();
		case "host_ended":
			return m.status_last_end_host_ended();
		case "host_error":
			return m.status_last_end_host_error();
		case "lost":
			return m.status_last_end_lost();
		case "stopped_by_operator":
			return m.status_last_end_stopped();
		default: {
			const unreached: never = s.ended;
			return unreached;
		}
	}
}

/**
 * One past session: the device, how long, when; its numbers fold under it. Copy hands over the
 * API's own answer, so what is pasted into an issue is what the host said.
 */
export const RecentSessionRow: FC<{ session: SessionSummary }> = ({
	session,
}) => {
	const [copied, setCopied] = useState(false);
	const span = session.bitrate;
	const facts = [
		session.mode,
		`${session.codec.toUpperCase()} ${session.bit_depth}-bit ${session.chroma}`,
		span
			? m.status_last_bitrate_avg({ mbit: fmtNumber(span.avg_kbps / 1000, 1) })
			: m.status_last_bitrate({
					mbit: fmtNumber(session.bitrate_kbps / 1000, 1),
				}),
		span &&
			span.adaptive_steps > 0 &&
			m.status_last_steps({ count: span.adaptive_steps }),
		session.frames_dropped != null &&
			m.status_last_dropped({ count: session.frames_dropped }),
		session.audio && m.status_last_audio_late({ count: session.audio.late }),
		session.gyro && m.status_last_gyro({ count: session.gyro.stalls }),
		session.hdr && "HDR",
		session.join && m.status_last_join(),
		m.status_last_ended({ reason: endedLabel(session) }),
	].filter(Boolean) as string[];
	const copy = () => {
		try {
			navigator.clipboard.writeText(JSON.stringify(session, null, 2));
			setCopied(true);
			setTimeout(() => setCopied(false), 1500);
		} catch {
			// Clipboard denied: the numbers are on screen, nothing worth interrupting for.
		}
	};
	return (
		<motion.li variants={ROW} className="py-3 first:pt-0 last:pb-0">
			<div className="flex flex-wrap items-baseline gap-x-3 gap-y-0.5 text-sm">
				<span className="font-medium">
					{session.client_name || session.client}
				</span>
				<span className="text-muted-foreground">
					{fmtSpan(session.duration_s)} ·{" "}
					{fmtAgo(session.started_unix + session.duration_s)} · {session.mode} ·{" "}
					{session.codec.toUpperCase()}
				</span>
			</div>
			<Details>
				<p className="text-xs text-muted-foreground tabular-nums">
					{facts.join(" · ")}
				</p>
				<Button variant="outline" size="sm" className="mt-2" onClick={copy}>
					{copied ? (
						<Check className="size-3.5 text-[var(--success)]" />
					) : (
						<Clipboard className="size-3.5" />
					)}
					{copied ? m.status_last_copied() : m.status_last_copy()}
				</Button>
			</Details>
		</motion.li>
	);
};

function stateLabel(state: string): string {
	switch (state) {
		case "launching":
			return m.games_state_launching();
		case "running":
			return m.games_state_running();
		case "exited":
			return m.games_state_exited();
		case "grace":
			return m.games_state_grace();
		case "untracked":
			return m.games_state_untracked();
		case "detached":
			return m.games_state_detached();
		default:
			return state;
	}
}

/** `untracked` is not an error, so it never wears `grace`'s destructive styling. */
function stateVariant(state: string): "success" | "secondary" | "outline" {
	if (state === "running") return "success";
	if (state === "launching") return "secondary";
	return "outline";
}
