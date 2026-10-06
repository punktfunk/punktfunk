// The **Whose library** chip: Library, Game sources and Plugins are per seat, so on a host with a
// seat of its own they show the box's by default and a seat's on request. Nothing else follows it.
import { QueryClientProvider, useQueryClient } from "@tanstack/react-query";
import { type FC, type ReactNode, useContext } from "react";
import { useGetHostInfo } from "@/api/gen/host/host";
import { useListProfiles } from "@/api/gen/profiles/profiles";
import {
	type SeatChoice,
	type SeatPage,
	SeatScopeContext,
	type SeatScopeValue,
	seatChoices,
	seatClient,
} from "@/api/seat";
import { QueryState } from "@/components/query-state";
import { Badge } from "@/components/ui/badge";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { useLocalPref } from "@/lib/prefs";
import { m } from "@/paraglide/messages";

/** The box's own entry in the chip. A profile id has no `-`, so no seat can be called this. */
const BOX = "-box-";

const isIdOrNull = (v: unknown): v is string | null =>
	v === null || typeof v === "string";

/**
 * Runs a page's queries against the picked seat (or the box) and offers the pick.
 *
 * The last pick per page is remembered in this browser. A pick that is no longer a seat falls
 * back to the box.
 */
export const SeatScope: FC<{ page: SeatPage; children: ReactNode }> = ({
	page,
	children,
}) => {
	const profiles = useListProfiles();
	const host = useGetHostInfo();
	const [stored, store] = useLocalPref<string | null>(
		`pf.seat.${page}`,
		null,
		isIdOrNull,
	);
	const seats = seatChoices(profiles.data, host.data?.os);
	const settled = !profiles.isPending && !host.isPending;
	return (
		// A remembered seat waits for the list that says it still is one.
		<QueryState isLoading={stored !== null && !settled} error={null}>
			<SeatScopeView
				page={page}
				seats={seats}
				seat={seats.find((s) => s.id === stored) ?? null}
				ownerName={
					profiles.data?.find((p) => p.owner)?.display_name ?? m.seat_box()
				}
				pick={store}
			>
				{children}
			</SeatScopeView>
		</QueryState>
	);
};

/** `SeatScope` without its queries. A seat gets its own cache, and a pick remounts the page. */
export const SeatScopeView: FC<SeatScopeValue & { children: ReactNode }> = ({
	children,
	...scope
}) => {
	const root = useQueryClient();
	const id = scope.seat?.id ?? null;
	return (
		<SeatScopeContext.Provider value={scope}>
			<QueryClientProvider
				key={id ?? BOX}
				client={id ? seatClient(id, root) : root}
			>
				{children}
			</QueryClientProvider>
		</SeatScopeContext.Provider>
	);
};

/** The pick. Absent while no seat has a host of its own. */
export const SeatChip: FC = () => {
	const scope = useContext(SeatScopeContext);
	if (!scope || scope.seats.length === 0) return null;
	const label =
		scope.page === "plugins" ? m.seat_whose_plugins() : m.seat_whose_library();
	return (
		<Select
			value={scope.seat?.id ?? BOX}
			onValueChange={(v) => scope.pick(v === BOX ? null : v)}
		>
			<SelectTrigger aria-label={label} className="w-auto gap-2">
				<span className="text-muted-foreground">{label}</span>
				<SelectValue />
			</SelectTrigger>
			<SelectContent>
				<SelectItem value={BOX}>{scope.ownerName}</SelectItem>
				{scope.seats.map((s: SeatChoice) => (
					<SelectItem key={s.id} value={s.id}>
						{s.name}
					</SelectItem>
				))}
			</SelectContent>
		</Select>
	);
};

/** Whose library an entry page edits, when it is a seat's. */
export const SeatNote: FC = () => {
	const seat = useContext(SeatScopeContext)?.seat;
	return seat ? (
		<Badge variant="secondary" className="self-start">
			{m.seat_note({ name: seat.name })}
		</Badge>
	) : null;
};
