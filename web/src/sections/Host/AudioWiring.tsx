import type { FC } from "react";
import { useGetStatus } from "@/api/gen/host/host";
import type { AudioWiring } from "@/api/gen/model/audioWiring";
import { Badge } from "@/components/ui/badge";
import { m } from "@/paraglide/messages";
import { Facts } from "./Strip";

/** Windows hosts report which endpoints carry game audio and the microphone; others report none. */
export const AudioWiringSection: FC = () => {
	const status = useGetStatus();
	const audio = status.data?.audio;
	return audio ? <AudioWiringFacts audio={audio} /> : null;
};

/**
 * One line per role plus a readiness badge. The degradation notes are spelled out: silent audio
 * or a vanished microphone is otherwise visible only in the host log. Home's Attention names the
 * unready state; this is where the facts live.
 */
export const AudioWiringFacts: FC<{ audio: AudioWiring }> = ({ audio }) => {
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
		<Facts
			label={
				<span className="inline-flex items-center gap-2">
					{m.audio_wiring_title()}
					<Badge variant={badge.variant}>{badge.text}</Badge>
				</span>
			}
		>
			<span className="text-sm font-medium">
				{m.audio_output()}: {audio.loopback ?? m.audio_unavailable()} ·{" "}
				{m.audio_microphone()}: {audio.mic ?? m.audio_unavailable()}
			</span>
			{notes.map((n) => (
				<span key={n} className="basis-full text-xs text-muted-foreground">
					{n}
				</span>
			))}
		</Facts>
	);
};
