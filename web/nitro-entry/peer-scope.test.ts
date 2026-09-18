// The same table as the host's `knock_sources_classify_by_address`, so the console and pairing
// never disagree about which peers are local.
import { describe, expect, it } from "bun:test";
import { isLocalPeer } from "./peer-scope.mjs";

describe("isLocalPeer", () => {
	it("admits this machine, the LAN and a tailnet", () => {
		for (const ip of [
			"127.0.0.1",
			"10.1.2.3",
			"172.16.0.1",
			"192.168.1.44",
			"169.254.3.4",
			"100.96.0.7",
			"::1",
			"0:0:0:0:0:0:0:1",
			"::ffff:192.168.1.44",
			"::ffff:c0a8:12c",
			"fd00::1",
			"FE80::1%eth0",
		]) {
			expect(isLocalPeer(ip), ip).toBe(true);
		}
	});

	it("refuses the internet and anything it cannot read", () => {
		for (const ip of [
			"203.0.113.5",
			"8.8.8.8",
			"172.32.0.1",
			"100.128.0.1",
			"0.0.0.0",
			"2606:4700::1111",
			"::ffff:203.0.113.5",
			"::",
			"",
			"not-an-ip",
			undefined,
		]) {
			expect(isLocalPeer(ip), String(ip)).toBe(false);
		}
	});
});
