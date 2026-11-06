// A host action is titled by its id; why it is unavailable by the code of the reason.
import { afterEach, expect, test } from "bun:test";
import type { ActionInfo } from "@/api/gen/model";
import { baseLocale, overwriteGetLocale } from "@/paraglide/runtime";
import { actionTitle, actionUnavailable } from "./PowerCard";

overwriteGetLocale(() => baseLocale);
afterEach(() => overwriteGetLocale(() => baseLocale));

const action = (over: Partial<ActionInfo>): ActionInfo =>
	({
		id: "power.sleep",
		title: "Sleep",
		available: false,
		...over,
	}) as ActionInfo;

test("the reason is worded by its code and the system's words stand alone", () => {
	const seat = action({
		unavailable_reason: "A seat can't sleep or shut down the host.",
		unavailable_code: "host_power_seat",
	});
	expect(actionUnavailable(seat)).toBe(
		"A seat can't sleep or shut down the host.",
	);
	overwriteGetLocale(() => "de");
	expect(actionUnavailable(seat)).toBe(
		"Ein Seat kann den Host nicht schlafen legen oder herunterfahren.",
	);
	const system = action({ unavailable_reason: "logind answered no" });
	expect(actionUnavailable(system)).toBe("logind answered no");
	expect(actionUnavailable(action({}))).toBeUndefined();
});

test("a known id is titled in the console's words, an unknown one by the host", () => {
	expect(actionTitle(action({ id: "display.next", title: "x" }))).toBe(
		"Next monitor",
	);
	expect(actionTitle(action({ id: "power.sleep", title: "x" }))).toBe(
		"Sleep host",
	);
	expect(actionTitle(action({ id: "from.newer", title: "Newer" }))).toBe(
		"Newer",
	);
});
