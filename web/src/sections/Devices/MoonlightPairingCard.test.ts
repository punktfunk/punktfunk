import { describe, expect, test } from "bun:test";
import type { PendingCeremony } from "@/api/gen/model/pendingCeremony";
import { addressedCeremony } from "./MoonlightPairingCard";

const knock = (uniqueid: string, fingerprint: string): PendingCeremony =>
	({ uniqueid, fingerprint, peer_ip: "192.168.1.9" }) as PendingCeremony;
const keyOf = (c: PendingCeremony) =>
	`${c.uniqueid}\u0000${c.fingerprint}\u0000${c.peer_ip}`;

describe("addressedCeremony", () => {
	const tv = knock("0123456789ABCDEF", "bb");
	const intruder = knock("0123456789ABCDEF", "aa");

	test("a sole knock needs no pick", () => {
		expect(addressedCeremony([tv], "")).toBe(tv);
	});

	test("a second knock never inherits the default", () => {
		expect(addressedCeremony([intruder, tv], "")).toBeUndefined();
	});

	test("the pick wins, and a vanished pick addresses nothing", () => {
		expect(addressedCeremony([intruder, tv], keyOf(tv))).toBe(tv);
		expect(addressedCeremony([intruder], keyOf(tv))).toBeUndefined();
	});
});
