// `/diagnostics/controllers` — the pad the host holds, lit by what it actually receives.
//
// The point of the page is that it is the HOST's copy: every light here is a state the host
// applied to the virtual controller, so a question like "does Guide ever reach the host from
// this pad" is answered by looking, not by an evdev dump on the box.
//
// Read-only, and open only while it is on screen — the host publishes nothing with nobody
// attached (`crates/punktfunk-host/src/pad_feed.rs`).

import { Copy, Pause, Play } from "lucide-react";
import { type FC, useEffect, useState } from "react";
import { useGetStatus } from "@/api/gen/host/host";
import type { PadFrame } from "@/api/gen/model/padFrame";
import type { SessionRow } from "@/api/gen/model/sessionRow";
import { QueryState } from "@/components/query-state";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { m } from "@/paraglide/messages";
import { PadDiagram } from "./PadDiagram";
import { logText, type PadLogLine } from "./pads";
import { usePadFeed } from "./usePadFeed";

/** How often the "last event" age is re-read. Finer than this is noise on a live page. */
const AGE_TICK_MS = 250;

export const SectionControllers: FC = () => {
	// The host-wide poll the rest of the console already runs; the pads themselves are a stream.
	const status = useGetStatus({ query: { refetchInterval: 5_000 } });
	const sessions = (status.data?.sessions ?? []).filter(
		(s): s is SessionRow & { id: number } => typeof s.id === "number",
	);
	return (
		<div className="flex flex-col gap-card">
			<p className="text-sm text-muted-foreground">{m.controllers_intro()}</p>
			<QueryState
				isLoading={status.isLoading}
				error={status.error}
				refetch={status.refetch}
			>
				{sessions.length === 0 ? (
					<Card>
						<CardContent>
							<p className="text-sm text-muted-foreground">
								{m.controllers_no_session()}
							</p>
						</CardContent>
					</Card>
				) : (
					<SessionPads sessions={sessions} />
				)}
			</QueryState>
		</div>
	);
};

/** One session at a time: a second stream would cost a second connection for nothing. */
const SessionPads: FC<{ sessions: (SessionRow & { id: number })[] }> = ({
	sessions,
}) => {
	const first = sessions[0];
	const [chosen, setChosen] = useState(first?.id);
	const session = sessions.find((s) => s.id === chosen) ?? first;
	const [paused, setPaused] = useState(false);
	const feed = usePadFeed(session?.id, paused);
	if (!session) return null;

	return (
		<div className="flex flex-col gap-card">
			{sessions.length > 1 && (
				<Select
					value={String(session.id)}
					onValueChange={(v) => setChosen(Number(v))}
				>
					<SelectTrigger className="w-72">
						<SelectValue />
					</SelectTrigger>
					<SelectContent>
						{sessions.map((s) => (
							<SelectItem key={s.id} value={String(s.id)}>
								{/* Whose session, where several people play. */}
								{s.profile
									? `${s.profile.display_name} · ${s.client_name ?? s.client}`
									: (s.client_name ?? s.client)}
							</SelectItem>
						))}
					</SelectContent>
				</Select>
			)}
			{feed.failed ? (
				<Card>
					<CardContent>
						<p className="text-sm text-destructive">
							{m.controllers_stream_failed()}
						</p>
					</CardContent>
				</Card>
			) : feed.pads.length === 0 ? (
				<Card>
					<CardContent>
						<p className="text-sm text-muted-foreground">
							{m.controllers_no_pads()}
						</p>
					</CardContent>
				</Card>
			) : (
				<div className="grid gap-card md:grid-cols-2">
					{feed.pads.map((pad) => (
						<PadCard key={pad.pad} frame={pad} client={session} />
					))}
				</div>
			)}
			<LogCard
				log={feed.log}
				paused={paused}
				onPause={() => setPaused((p) => !p)}
			/>
		</div>
	);
};

/** One pad: the drawing, then the line that says which device it is and how live it is. */
const PadCard: FC<{ frame: PadFrame; client: SessionRow }> = ({
	frame,
	client,
}) => (
	<Card>
		<CardHeader>
			<CardTitle className="flex items-center gap-2">
				{m.controllers_pad({ n: frame.pad })}
				<Badge variant="secondary">{frame.device}</Badge>
			</CardTitle>
		</CardHeader>
		<CardContent className="flex flex-col gap-3">
			<PadDiagram frame={frame} />
			<dl className="flex flex-wrap gap-x-4 gap-y-1 text-xs text-muted-foreground">
				<Fact label={m.controllers_client()}>
					{client.client_name ?? client.client}
				</Fact>
				{frame.slot !== undefined && (
					<Fact label={m.controllers_slot()}>{frame.slot}</Fact>
				)}
				{frame.declared && frame.declared !== frame.device && (
					<Fact label={m.controllers_declared()}>{frame.declared}</Fact>
				)}
				<Fact label={m.controllers_last_event()}>
					<Age ts={frame.ts_ms} />
				</Fact>
			</dl>
		</CardContent>
	</Card>
);

const Fact: FC<{ label: string; children: React.ReactNode }> = ({
	label,
	children,
}) => (
	<div className="flex gap-1">
		<dt>{label}</dt>
		<dd className="font-medium text-foreground tabular-nums">{children}</dd>
	</div>
);

/** "N ms ago", ticking on its own: a silent pad is the answer to half the questions here. */
const Age: FC<{ ts: number }> = ({ ts }) => {
	const [now, setNow] = useState(() => Date.now());
	useEffect(() => {
		const t = setInterval(() => setNow(Date.now()), AGE_TICK_MS);
		return () => clearInterval(t);
	}, []);
	return <>{m.controllers_ms_ago({ ms: Math.max(0, now - ts) })}</>;
};

/** The tail, bounded to `LOG_MAX` lines, with the block Copy hands to an issue. */
const LogCard: FC<{
	log: PadLogLine[];
	paused: boolean;
	onPause: () => void;
}> = ({ log, paused, onPause }) => {
	const [copied, setCopied] = useState(false);
	const copy = () => {
		navigator.clipboard
			.writeText(logText(log))
			.then(() => setCopied(true))
			.catch(() => setCopied(false));
	};
	return (
		<Card>
			<CardHeader className="flex flex-row items-center justify-between">
				<CardTitle>{m.controllers_log()}</CardTitle>
				<div className="flex gap-2">
					<Button variant="outline" size="sm" onClick={onPause}>
						{paused ? (
							<Play className="size-4" />
						) : (
							<Pause className="size-4" />
						)}
						{paused ? m.controllers_resume() : m.controllers_pause()}
					</Button>
					<Button
						variant="outline"
						size="sm"
						onClick={copy}
						disabled={log.length === 0}
					>
						<Copy className="size-4" />
						{copied ? m.controllers_copied() : m.controllers_copy()}
					</Button>
				</div>
			</CardHeader>
			<CardContent>
				{log.length === 0 ? (
					<p className="text-sm text-muted-foreground">
						{m.controllers_log_empty()}
					</p>
				) : (
					// Newest last, and scrolled to the bottom: the reader is watching the tail.
					<ol className="flex max-h-80 flex-col overflow-y-auto font-mono text-xs">
						{log.map((l) => (
							<li key={l.seq} className="flex gap-2 tabular-nums">
								<span className="text-muted-foreground">
									{new Date(l.ts_ms).toTimeString().slice(0, 8)}
								</span>
								<span className="text-muted-foreground">pad {l.pad}</span>
								<span>{l.text}</span>
							</li>
						))}
					</ol>
				)}
			</CardContent>
		</Card>
	);
};
