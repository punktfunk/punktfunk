// The host's sentences by message id: seat lines, seat checks, the door and the update result.
import { afterEach, expect, test } from "bun:test";
import type { HostInfo } from "@/api/gen/model/hostInfo";
import type { SeatCheck } from "@/api/gen/model/seatCheck";
import type { SeatPublic } from "@/api/gen/model/seatPublic";
import type { UpdateResultInfo } from "@/api/gen/model/updateResultInfo";
import { baseLocale, overwriteGetLocale } from "@/paraglide/runtime";
import {
	doorUnavailable,
	hostMessage,
	seatCheckText,
	seatDetail,
	updateError,
} from "./host-messages";

overwriteGetLocale(() => baseLocale);
afterEach(() => overwriteGetLocale(() => baseLocale));

const seat = (over: Partial<SeatPublic>): SeatPublic => over as SeatPublic;
const check = (over: Partial<SeatCheck>): SeatCheck => over as SeatCheck;

test("a message id words its sentence from params", () => {
	expect(hostMessage("apierr_host_power_seat")).toBe(
		"A seat can't sleep or shut down the host.",
	);
	expect(hostMessage("no_such_message")).toBeUndefined();
	expect(hostMessage("constructor")).toBeUndefined();
});

test("a seat's state line is worded by its id", () => {
	const line = seat({
		detail: "Signing in to the seat's Windows account (try 2 of 3)…",
		detail_id: "seatline_stage_sign_in_try",
		detail_params: { attempt: "2", total: "3" },
	});
	expect(seatDetail(line)).toBe(
		"Signing in to the seat's Windows account (try 2 of 3)…",
	);
	overwriteGetLocale(() => "de");
	expect(seatDetail(line)).toBe(
		"Anmeldung beim Windows-Konto des Seats (Versuch 2 von 3)…",
	);
	// A line without an id, or with an id from a newer host, keeps the host's English.
	expect(seatDetail(seat({ detail: "Waiting." }))).toBe("Waiting.");
	expect(
		seatDetail(seat({ detail: "Newer.", detail_id: "seatline_new" })),
	).toBe("Newer.");
	expect(seatDetail(seat({}))).toBeUndefined();
});

test("a seat check takes its count as a string and picks the plural", () => {
	const days = (n: string) =>
		check({
			message: "Remote Desktop runs 12 more days without a license server.",
			id: "seatcheck_rds_grace_days",
			params: { count: n },
		});
	expect(seatCheckText(days("1"))).toBe(
		"Remote Desktop runs 1 more day without a license server.",
	);
	expect(seatCheckText(days("12"))).toBe(
		"Remote Desktop runs 12 more days without a license server.",
	);
	expect(seatCheckText(check({ message: "Plain.", code: "x" }))).toBe("Plain.");
});

test("the door's reason is an API error message", () => {
	const door = (over: Partial<HostInfo>): HostInfo => over as HostInfo;
	expect(
		doorUnavailable(
			door({
				door_unavailable: "The system's own words.",
				door_unavailable_code: "door_missing_package",
				door_unavailable_params: {
					command: "sudo apt install punktfunk-seats",
				},
			}),
		),
	).toBe(
		"This install is missing the punktfunk-seats package. Install it: sudo apt install punktfunk-seats",
	);
	expect(doorUnavailable(door({ door_unavailable: "Only words." }))).toBe(
		"Only words.",
	);
	expect(doorUnavailable(door({}))).toBeUndefined();
});

test("a failed update fills its message from the result's own versions", () => {
	const result = (over: Partial<UpdateResultInfo>): UpdateResultInfo =>
		({ ok: false, from: "0.46.0", to: "0.47.0", ...over }) as UpdateResultInfo;
	const rolledBack = result({
		error: "The host restarted still running 0.46.0 (expected 0.47.0).",
		error_code: "rolled_back",
	});
	expect(updateError(rolledBack)).toBe(
		"The host restarted still running 0.46.0 (expected 0.47.0) — the installer aborted or rolled back; see its log",
	);
	overwriteGetLocale(() => "de");
	expect(updateError(rolledBack)).toContain(
		"läuft noch mit 0.46.0 (erwartet: 0.47.0)",
	);
	expect(updateError(result({ error: "Disk full." }))).toBe("Disk full.");
	expect(updateError(result({ error: "Odd.", error_code: "from_newer" }))).toBe(
		"Odd.",
	);
});
