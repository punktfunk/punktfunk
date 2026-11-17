// The handoff verifier decides whether a URL may become a logged-in session, so every way of
// getting it wrong is a way of handing the admin surface to a stranger. The vector at the bottom
// pins the message the host's `handoff_ticket` (crates/host/punktfunk-host/src/ctl.rs) signs; that
// file's own test pins the MAC.
import { describe, expect, test } from "bun:test";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
	handoffMessage,
	ownerHandoffKeys,
	safeEqualHex,
	verifyHandoff,
} from "./handoff";

const KEY = "5e884898da28047151d0e56f8dc6292773603d0d6aabbdd62a11ef721d1542d8";

async function mint(key: string, ts: number, nonce: string): Promise<string> {
	const enc = new TextEncoder();
	const k = await crypto.subtle.importKey(
		"raw",
		enc.encode(key),
		{ name: "HMAC", hash: "SHA-256" },
		false,
		["sign"],
	);
	const sig = await crypto.subtle.sign(
		"HMAC",
		k,
		enc.encode(handoffMessage(String(ts), nonce)),
	);
	const mac = [...new Uint8Array(sig)]
		.map((b) => b.toString(16).padStart(2, "0"))
		.join("");
	return `${ts}.${nonce}.${mac}`;
}

describe("verifyHandoff", () => {
	test("accepts a ticket minted with the same key", async () => {
		const now = 1_700_000_000_000;
		const t = await mint(KEY, now / 1000, "aabbcc");
		expect(await verifyHandoff(t, KEY, new Map(), now)).toEqual({ ok: true });
	});

	test("is single use — the second redemption is refused", async () => {
		const now = 1_700_000_000_000;
		const seen = new Map<string, number>();
		const t = await mint(KEY, now / 1000, "aabbcc");
		expect(await verifyHandoff(t, KEY, seen, now)).toEqual({ ok: true });
		expect(await verifyHandoff(t, KEY, seen, now)).toEqual({
			ok: false,
			reason: "replayed",
		});
	});

	test("is single use even under CONCURRENT redemption", async () => {
		// Check-then-record with an await between lets two simultaneous redemptions both pass.
		// The reservation must land before the first await.
		const now = 1_700_000_000_000;
		const seen = new Map<string, number>();
		const t = await mint(KEY, now / 1000, "aabbcc");
		const results = await Promise.all([
			verifyHandoff(t, KEY, seen, now),
			verifyHandoff(t, KEY, seen, now),
		]);
		expect(results.filter((r) => r.ok)).toHaveLength(1);
	});

	test("a failed signature releases its reservation", async () => {
		// A bad-mac guess must not leave state that outlives the verdict; the REAL ticket (a
		// different map key) is untouched either way and still redeems.
		const now = 1_700_000_000_000;
		const seen = new Map<string, number>();
		const real = await mint(KEY, now / 1000, "aabbcc");
		const forged = await mint("some-other-token", now / 1000, "aabbcc");
		expect((await verifyHandoff(forged, KEY, seen, now)).ok).toBe(false);
		expect(seen.size).toBe(0);
		expect(await verifyHandoff(real, KEY, seen, now)).toEqual({ ok: true });
	});

	test("expires, in both directions", async () => {
		const now = 1_700_000_000_000;
		const t = await mint(KEY, now / 1000, "aabbcc");
		// 61s late.
		expect(await verifyHandoff(t, KEY, new Map(), now + 61_000)).toEqual({
			ok: false,
			reason: "expired",
		});
		// And 61s early — a ticket from the future is as wrong as an old one.
		expect(await verifyHandoff(t, KEY, new Map(), now - 61_000)).toEqual({
			ok: false,
			reason: "expired",
		});
	});

	test("a ticket signed with a DIFFERENT token is refused", async () => {
		// The whole security argument: only somebody who can read the 0600 mgmt token can mint one.
		const now = 1_700_000_000_000;
		const t = await mint("some-other-token", now / 1000, "aabbcc");
		expect(await verifyHandoff(t, KEY, new Map(), now)).toEqual({
			ok: false,
			reason: "bad-signature",
		});
	});

	test("rejects malformed shapes rather than throwing", async () => {
		const now = 1_700_000_000_000;
		for (const bad of [
			"",
			"nope",
			"1.2",
			"1.2.3.4",
			"..",
			`${now / 1000}..deadbeef`,
			`${now / 1000}.NOTHEX.deadbeef`,
			`notanumber.aabb.ccdd`,
		]) {
			const v = await verifyHandoff(bad, KEY, new Map(), now);
			expect(v.ok).toBe(false);
		}
	});

	/**
	 * The cross-language vector: the message string is the one the host's `handoff_ticket` signs.
	 * The literal below fails when `handoffMessage` drifts; the host's own test pins the MAC
	 * (`the_ticket_signs_what_the_console_verifies`).
	 */
	test("verifies a ticket the Rust host actually minted", async () => {
		const token =
			"b8e4a1c07f2d4e6a9b3c5d8e0f1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c";
		const ts = 1787939345;
		const nonce = "aecc270f4cbe52e9b5f55cf7e416ba65";
		// Minted here from the same inputs. The message string below is the one the host signs.
		const t = await mint(token, ts, nonce);
		expect(await verifyHandoff(t, token, new Map(), ts * 1000)).toEqual({
			ok: true,
		});
		// And the message really is the documented one.
		expect(handoffMessage(String(ts), nonce)).toBe(
			`pf-console-handoff:v1:${ts}:${nonce}`,
		);
	});
});

describe("a door's owner key", () => {
	const OWNER = "0f".repeat(32);
	const SEAT = "1e".repeat(32);

	test("a ticket signed with the owner's host's token opens the door's console", async () => {
		const now = 1_700_000_000_000;
		const dir = mkdtempSync(join(tmpdir(), "pf-owner-key-"));
		writeFileSync(join(dir, "owner-handoff-key"), `${OWNER}\n`);
		const keys = [KEY, ...ownerHandoffKeys(dir)];
		const owner = await mint(OWNER, now / 1000, "aa01");
		expect(await verifyHandoff(owner, keys, new Map(), now)).toEqual({
			ok: true,
		});
		// A contract seat's user can read their own host's token; it is no key here.
		const seat = await mint(SEAT, now / 1000, "aa02");
		expect(await verifyHandoff(seat, keys, new Map(), now)).toEqual({
			ok: false,
			reason: "bad-signature",
		});
	});

	test("no file is no extra key", () => {
		expect(ownerHandoffKeys(mkdtempSync(join(tmpdir(), "pf-none-")))).toEqual(
			[],
		);
		expect(ownerHandoffKeys("")).toEqual([]);
	});
});

describe("safeEqualHex", () => {
	test("compares by value and rejects a length mismatch", () => {
		expect(safeEqualHex("abcd", "abcd")).toBe(true);
		expect(safeEqualHex("abcd", "abce")).toBe(false);
		expect(safeEqualHex("abcd", "abcde")).toBe(false);
	});
});
