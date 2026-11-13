// Whose page it is: Library, Plugins and Automation offer a pick in their title (R10), one for
// the whole console. Library shows any profile's view of the box's library; Plugins and
// Automation a seat's own host. A page that has nothing of the pick's shows the owner's.
import { QueryClientProvider, useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { type FC, type ReactNode, useContext, useEffect } from "react";
import { useGetHostInfo } from "@/api/gen/host/host";
import {
	getListProfilesQueryKey,
	useListProfiles,
} from "@/api/gen/profiles/profiles";
import {
	onProfileGone,
	personScope,
	type SeatPage,
	SeatScopeContext,
	type SeatScopeValue,
	scopeChoices,
	seatClient,
} from "@/api/seat";
import { QueryState } from "@/components/query-state";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { apiErrorMessage } from "@/lib/errors";
import { usePerson } from "@/lib/person";
import { useLocalPref } from "@/lib/prefs";
import { m } from "@/paraglide/messages";

/** The owner's entry in the pick. A profile id has no `-`, so no profile can be called this. */
const OWNER = "-owner-";

/** The console's pick, a profile id; `null` is the owner. */
export const PICK_KEY = "pf.profile";

const isIdOrNull = (v: unknown): v is string | null =>
	v === null || typeof v === "string";

/**
 * Runs a page's queries against the pick and offers it. In a person's own console the page is
 * theirs alone, with no pick and no read of the box's profiles.
 */
export const SeatScope: FC<{ page: SeatPage; children: ReactNode }> = ({
	page,
	children,
}) => {
	const person = usePerson();
	if (person)
		return (
			<SeatScopeView {...personScope(page, person)}>{children}</SeatScopeView>
		);
	return <OperatorScope page={page}>{children}</OperatorScope>;
};

/**
 * The operator's pick, remembered once per browser; one that is not on this page's list shows
 * the owner's.
 */
const OperatorScope: FC<{ page: SeatPage; children: ReactNode }> = ({
	page,
	children,
}) => {
	const profiles = useListProfiles();
	const host = useGetHostInfo();
	const [stored, store] = useLocalPref<string | null>(
		PICK_KEY,
		null,
		isIdOrNull,
	);
	const door = host.data?.door === true;
	const choices = scopeChoices(page, profiles.data, host.data?.os, door);
	const settled = !profiles.isPending && !host.isPending;
	return (
		// A remembered pick waits for the list that says it still is one, and a door for the host
		// that says it is one: before that the page would ask the door for a library. Automation's
		// owner is the box's own on a door too.
		<QueryState
			isLoading={
				(stored !== null ||
					(page !== "automation" && host.data?.door !== false)) &&
				!settled
			}
			error={null}
		>
			<SeatScopeView
				page={page}
				choices={choices}
				current={
					choices.find((c) => c.id !== null && c.id === stored) ??
					choices[0] ??
					null
				}
				ownerId={profiles.data?.find((p) => p.owner)?.id}
				door={door}
				pick={store}
			>
				{children}
			</SeatScopeView>
		</QueryState>
	);
};

/**
 * `SeatScope` without its queries. A seat gets its own cache, and a pick that changes the seat
 * remounts the page; a profile's view of the library keeps it and names the profile in `as`. A
 * profile the host no longer has turns the pick back to the owner at once, with its sentence.
 */
export const SeatScopeView: FC<SeatScopeValue & { children: ReactNode }> = ({
	children,
	...scope
}) => {
	const root = useQueryClient();
	const id = scope.current?.seat?.id ?? null;
	const client = id ? seatClient(id, root) : root;
	const as = scope.current?.as;
	const { pick } = scope;
	useEffect(() => {
		if (!as) return;
		return onProfileGone(client, (e, read) => {
			// A failed write already says so in its own toast.
			if (read)
				toast.error(apiErrorMessage(e) ?? m.common_error(), {
					id: "profile-gone",
				});
			pick(null);
			void root.invalidateQueries({ queryKey: getListProfilesQueryKey() });
		});
	}, [client, root, as, pick]);
	return (
		<SeatScopeContext.Provider value={scope}>
			<QueryClientProvider key={id ?? OWNER} client={client}>
				{children}
			</QueryClientProvider>
		</SeatScopeContext.Provider>
	);
};

const OF: Record<SeatPage, (name: string) => string> = {
	library: (name) => m.seat_library_of({ name }),
	plugins: (name) => m.seat_plugins_of({ name }),
	automation: (name) => m.seat_automation_of({ name }),
};

const WHOSE: Record<SeatPage, () => string> = {
	library: () => m.seat_whose_library(),
	plugins: () => m.seat_whose_plugins(),
	automation: () => m.seat_whose_automation(),
};

/**
 * The page's title, which is also the pick: **Anna's library ▾**. Plain `title` while the page
 * has one choice or none.
 */
export const SeatTitle: FC<{ title: string }> = ({ title }) => {
	const scope = useContext(SeatScopeContext);
	const heading = "text-2xl font-semibold";
	if (!scope || scope.choices.length < 2)
		return <h1 className={heading}>{title}</h1>;
	const of = OF[scope.page];
	return (
		<h1 className={heading}>
			<Select
				value={scope.current?.id ?? OWNER}
				onValueChange={(v) => scope.pick(v === OWNER ? null : v)}
			>
				<SelectTrigger
					aria-label={WHOSE[scope.page]()}
					className="h-auto w-auto max-w-full gap-2 border-0 bg-transparent p-0 text-2xl font-semibold shadow-none dark:bg-transparent"
				>
					<SelectValue />
				</SelectTrigger>
				<SelectContent>
					{scope.choices.map((c) => (
						<SelectItem key={c.id ?? OWNER} value={c.id ?? OWNER}>
							{of(c.name)}
						</SelectItem>
					))}
				</SelectContent>
			</Select>
		</h1>
	);
};
