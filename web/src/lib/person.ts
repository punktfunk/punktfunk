// A person's own console: whose it is, and the signal that it closed. The server decides every
// call (server/util/person.ts); this only picks what the page draws.
import { createContext, useContext } from "react";
import type { PersonMe } from "../../server/routes/_auth/person/me.get";

export type { PersonMe };

/** The cookie the server sets beside a person's session. Readable by the page, it grants nothing. */
const MODE = /(?:^|;\s*)pf_mode=person(?:;|$)/;

/** True when `cookie` marks a person's console. */
export const isPersonCookie = (cookie: string | null | undefined): boolean =>
	MODE.test(cookie ?? "");

/** True in a browser whose console is a person's. */
export const personBrowser = (): boolean =>
	typeof document !== "undefined" && isPersonCookie(document.cookie);

/** Why a person's console closed. Each reads as its own sentence on the closed page. */
export const CLOSED = [
	"refused",
	"wait",
	"ended",
	"stopped",
	"signed-out",
] as const;
export type Closed = (typeof CLOSED)[number];

/** The server's code for a session that ended under the person, as the closed page names it. */
export function closedBy(code: unknown): Closed | null {
	if (code === "person_session_ended") return "ended";
	if (code === "person_seat_stopped") return "stopped";
	return null;
}

/** Fired on `window` when a call learns the person's console closed; `detail` is a `Closed`. */
export const CLOSED_EVENT = "pf-person-closed";

export function announceClosed(why: Closed): void {
	if (typeof window === "undefined") return;
	window.dispatchEvent(new CustomEvent(CLOSED_EVENT, { detail: why }));
}

/** Whose console this is; `null` in the operator's. */
export const PersonContext = createContext<PersonMe | null>(null);

export const usePerson = (): PersonMe | null => useContext(PersonContext);
