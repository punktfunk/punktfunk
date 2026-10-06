// The **Whose library** chip: Library, Game sources and Plugins are per seat, so on a host with a
// seat of its own they show the box's by default and a seat's on request. Nothing else follows it.
// A door has no library of its own: its pages show the owner's seat unless another is picked.
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
	const door = host.data?.door === true;
	const seats = seatChoices(profiles.data, host.data?.os, door);
	const settled = !profiles.isPending && !host.isPending;
	return (
		// A remembered seat waits for the list that says it still is one, and a door for the
		// host that says it is one: before that the page would ask the door for a library.
		<QueryState
			isLoading={(stored !== null || host.data?.door !== false) && !settled}
			error={null}
		>
			<SeatScopeView
				page={page}
				seats={seats}
				seat={
					seats.find((s) => s.id === stored) ??
					(door ? seats[0] : undefined) ??
					null
				}
				ownerName={
					profiles.data?.find((p) => p.owner)?.display_name ?? m.seat_box()
				}
				door={door}
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
	// A door with only the owner's seat has nothing to choose.
	if (scope.door && scope.seats.length < 2) return null;
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
				{!scope.door && <SelectItem value={BOX}>{scope.ownerName}</SelectItem>}
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
	const scope = useContext(SeatScopeContext);
	const seat = scope?.seat;
	return seat && !(scope?.door && scope.seats.length < 2) ? (
		<Badge variant="secondary" className="self-start">
			{m.seat_note({ name: seat.name })}
		</Badge>
	) : null;
};
