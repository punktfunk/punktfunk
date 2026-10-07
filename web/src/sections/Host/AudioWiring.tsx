import { Volume2 } from "lucide-react";
import type { FC } from "react";
import { useGetStatus } from "@/api/gen/host/host";
import type { AudioWiring } from "@/api/gen/model/audioWiring";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { m } from "@/paraglide/messages";

/** Windows hosts report which endpoints carry game audio and the microphone; others report none. */
export const AudioWiringSection: FC = () => {
	const status = useGetStatus();
	const audio = status.data?.audio;
	return audio ? <AudioWiringCard audio={audio} /> : null;
};

/**
 * One line per role plus a readiness badge. The degradation notes are spelled out: silent audio
 * or a vanished microphone is otherwise visible only in the host log. Home's Attention names the
 * unready state; this is where the facts live.
 */
export const AudioWiringCard: FC<{ audio: AudioWiring }> = ({ audio }) => {
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
					<div>
						<dt className="text-xs text-muted-foreground">
							{m.audio_output()}
						</dt>
						<dd className="mt-0.5 font-medium">
							{audio.loopback ?? m.audio_unavailable()}
						</dd>
					</div>
					<div>
						<dt className="text-xs text-muted-foreground">
							{m.audio_microphone()}
						</dt>
						<dd className="mt-0.5 font-medium">
							{audio.mic ?? m.audio_unavailable()}
						</dd>
					</div>
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
