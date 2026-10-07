// **Other desktops**: every full seat's desktop, below the owner's. A seat carries no display
// policy — the seat contract (one virtual screen, never a real monitor) is the one line above
// the rows — so a row is state, not settings. Absent while no seat has a desktop of its own (R9).
import { useQueryClient } from "@tanstack/react-query";
import { motion } from "motion/react";
import type { FC } from "react";
import { useGetDisplayState } from "@/api/gen/display/display";
import { useGetHostInfo } from "@/api/gen/host/host";
import type { ProfileAdmin } from "@/api/gen/model/profileAdmin";
import { useListProfiles } from "@/api/gen/profiles/profiles";
import { isFullSeat, seatClient } from "@/api/seat";
import { ProfileAvatar } from "@/components/profile-avatar";
import { ROW, ROW_GAP, staggerProps } from "@/components/stagger";
import { Card, CardContent, CardTitle } from "@/components/ui/card";
import { m } from "@/paraglide/messages";

export const OtherDesktops: FC = () => {
	const profiles = useListProfiles({ query: { retry: false } });
	const host = useGetHostInfo();
	const door = host.data?.door === true;
	const seats = (profiles.data ?? []).filter((p) =>
		isFullSeat(p, host.data?.os, door),
	);
	if (seats.length === 0) return null;
	const owner =
		profiles.data?.find((p) => p.owner)?.display_name ?? m.seat_box();
	return (
		<Card>
			<CardContent className="space-y-3">
				<CardTitle>
					<h2>{m.display_other_desktops()}</h2>
				</CardTitle>
				<p className="text-sm text-muted-foreground">
					{m.display_seats_contract({ owner })}
				</p>
				<motion.ul {...staggerProps(ROW_GAP)} className="divide-y">
					{seats.map((p) => (
						<DesktopRow key={p.id} profile={p} />
					))}
				</motion.ul>
			</CardContent>
		</Card>
	);
};

const seatState = (p: ProfileAdmin): string => {
	const seat = p.seat;
	switch (seat?.state) {
		case "occupied":
			return m.profiles_occupied_by({ device: seat.occupant ?? "—" });
		case "starting":
			return seat.detail || m.profiles_state_starting();
		case "ready":
			return m.profiles_state_ready();
		case "unavailable":
			return seat.detail || m.profiles_state_unavailable();
		default:
			return m.profiles_state_stopped();
	}
};

/** One seat's desktop. Its screen's mode comes from the seat's own host, through the box. */
const DesktopRow: FC<{ profile: ProfileAdmin }> = ({ profile }) => {
	const root = useQueryClient();
	const occupied = profile.seat?.state === "occupied";
	const state = useGetDisplayState(
		{ query: { enabled: occupied, refetchInterval: 15_000, retry: false } },
		seatClient(profile.id, root),
	);
	const mode = occupied
		? state.data?.displays?.find((d) => d.state === "active")?.mode
		: undefined;
	return (
		<motion.li
			variants={ROW}
			className="flex flex-wrap items-center gap-x-3 gap-y-1 py-3"
		>
			<ProfileAvatar profile={profile} className="size-7 text-xs" />
			<div className="min-w-0 flex-1 basis-40">
				<div className="truncate font-medium">
					{m.display_desktop_of({ name: profile.display_name })}
				</div>
				{mode && (
					<div className="truncate text-xs text-muted-foreground">{mode}</div>
				)}
			</div>
			<span className="text-sm text-muted-foreground">
				{seatState(profile)}
			</span>
		</motion.li>
	);
};
