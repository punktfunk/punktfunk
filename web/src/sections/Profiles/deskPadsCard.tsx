// **Controllers on this PC**: each controller plugged into the host, and who plays with it.
import { Gamepad2 } from "lucide-react";
import type { FC } from "react";
import type { DeskPad } from "@/api/gen/model/deskPad";
import type { PadFrame } from "@/api/gen/model/padFrame";
import type { ProfileAdmin } from "@/api/gen/model/profileAdmin";
import {
	Card,
	CardContent,
	CardDescription,
	CardHeader,
	CardTitle,
} from "@/components/ui/card";
import { Label } from "@/components/ui/label";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";
import { PadDiagram } from "@/sections/Diagnostics/Controllers/PadDiagram";
import {
	DESK,
	holderOptions,
	holderValue,
	padArtKey,
	padNote,
	tellApart,
} from "./deskPads";

/** What the card needs beyond the profiles: the controllers and the one change. */
export type PadLending = {
	pads: DeskPad[];
	/** The controller a change is in flight for; its select waits. */
	busyKey: string | null;
	/** Gives `pad` to the profile `id`, or back to this PC for `null`. */
	onGive: (pad: DeskPad, id: string | null) => void;
};

/** A controller at rest, for the drawing. */
const idle = (device: string): PadFrame => ({
	device,
	buttons: 0,
	left_trigger: 0,
	right_trigger: 0,
	ls_x: 0,
	ls_y: 0,
	rs_x: 0,
	rs_y: 0,
	pad: 0,
	present: true,
	ts_ms: 0,
});

export const DeskPadsCard: FC<
	PadLending & { profiles: readonly ProfileAdmin[] }
> = ({ pads, profiles, busyKey, onGive }) => (
	<Card>
		<CardHeader>
			<CardTitle className="flex items-center gap-2">
				<Gamepad2 className="size-4" aria-hidden />
				{m.profiles_pads_title()}
			</CardTitle>
			<CardDescription>{m.profiles_pads_hint()}</CardDescription>
		</CardHeader>
		<CardContent>
			<ul className="divide-y">
				{pads.map((pad) => (
					<PadRow
						key={pad.key}
						pad={pad}
						all={pads}
						profiles={profiles}
						busy={busyKey === pad.key}
						onGive={onGive}
					/>
				))}
			</ul>
		</CardContent>
	</Card>
);

const PadRow: FC<{
	pad: DeskPad;
	all: readonly DeskPad[];
	profiles: readonly ProfileAdmin[];
	busy: boolean;
	onGive: PadLending["onGive"];
}> = ({ pad, all, profiles, busy, onGive }) => {
	const art = padArtKey(pad);
	const note = padNote(pad, profiles);
	const tell = tellApart(pad, all);
	const id = `pad-holder-${pad.key}`;
	return (
		<li className="flex flex-col gap-3 py-3 first:pt-0 last:pb-0 sm:flex-row sm:items-center">
			<div className="flex min-w-0 flex-1 items-center gap-3">
				{/* Decoration: the name beside it says what it is. */}
				<span
					aria-hidden
					className={cn(
						"flex w-20 shrink-0 justify-center text-muted-foreground",
						!pad.present && "opacity-50",
					)}
				>
					{art ? (
						<PadDiagram frame={idle(art)} />
					) : (
						<Gamepad2 className="size-8" />
					)}
				</span>
				<div className="min-w-0">
					<p className="flex items-baseline gap-2 text-sm font-medium">
						<span className="min-w-0 break-words">{pad.name}</span>
						{tell && (
							<span className="shrink-0 text-xs font-normal text-muted-foreground">
								{tell}
							</span>
						)}
					</p>
					{note && <p className="text-xs text-muted-foreground">{note}</p>}
				</div>
			</div>
			<div className="space-y-1 sm:w-52">
				<Label htmlFor={id} className="text-xs text-muted-foreground">
					{m.profiles_pads_used_by()}
				</Label>
				<Select
					value={holderValue(pad)}
					disabled={busy}
					onValueChange={(v) => onGive(pad, v === DESK ? null : v)}
				>
					<SelectTrigger id={id}>
						<SelectValue />
					</SelectTrigger>
					<SelectContent>
						{holderOptions(pad, profiles).map((o) => (
							<SelectItem key={o.value} value={o.value}>
								{o.label}
							</SelectItem>
						))}
					</SelectContent>
				</Select>
			</div>
		</li>
	);
};
