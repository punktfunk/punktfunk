import { expect, test } from "bun:test";
import type { NativeClient } from "@/api/gen/model/nativeClient";
import type { PairedClient } from "@/api/gen/model/pairedClient";
import type { SessionRow } from "@/api/gen/model/sessionRow";
import {
	deviceName,
	hasAccess,
	isStreaming,
	type PairedRow,
	pairedListView,
	pairedRows,
	pendingFingerprint,
	planesWithRows,
	renameBody,
	rowControls,
	storedAccess,
} from "./pairedRows";

const native = (over: Partial<NativeClient> = {}): NativeClient => ({
	fingerprint: "aa11",
	name: "Deck",
	until_disconnect: false,
	...over,
});
const moonlight = (over: Partial<PairedClient> = {}): PairedClient => ({
	fingerprint: "bb22",
	...over,
});
const nativeRow = (over: Partial<PairedRow> = {}): PairedRow => ({
	protocol: "native",
	fingerprint: "aa11",
	name: "Deck",
	...over,
});
const moonlightRow = (over: Partial<PairedRow> = {}): PairedRow => ({
	protocol: "moonlight",
	fingerprint: "bb22",
	name: "",
	...over,
});

test("native rows come first and carry the access fields", () => {
	const rows = pairedRows(
		[
			native({
				access_level: "play",
				grants: 3,
				expires_unix: 99,
				until_disconnect: true,
			}),
		],
		[moonlight({ label: "TV" })],
	);
	expect(rows).toEqual([
		{
			protocol: "native",
			fingerprint: "aa11",
			name: "Deck",
			accessLevel: "play",
			grants: 3,
			expiresUnix: 99,
			untilDisconnect: true,
		},
		{ protocol: "moonlight", fingerprint: "bb22", name: "TV", label: "TV" },
	]);
});

test("a Moonlight name is its label, else its certificate subject, else empty", () => {
	const name = (c: Partial<PairedClient>) =>
		pairedRows([], [moonlight(c)])[0]?.name;
	expect(name({ label: "TV", subject: "CN=NVIDIA GameStream Client" })).toBe(
		"TV",
	);
	expect(name({ label: null, subject: "CN=NVIDIA GameStream Client" })).toBe(
		"CN=NVIDIA GameStream Client",
	);
	expect(name({ subject: "CN=x" })).toBe("CN=x");
	expect(name({})).toBe("");
	// pins current behaviour: an empty label does not fall back to the subject.
	expect(name({ label: "", subject: "CN=x" })).toBe("");
});

test("a list that has not loaded contributes no rows", () => {
	expect(pairedRows(undefined, undefined)).toEqual([]);
	expect(pairedRows(undefined, [moonlight()])).toHaveLength(1);
	expect(pairedRows([native()], undefined)).toHaveLength(1);
});

const loaded = <T>(data: T[]) => ({ data, isLoading: false, error: null });
const failedWith = <T>(error: unknown, data?: T[]) => ({
	data,
	isLoading: false,
	error,
});
const loading = { isLoading: true, error: null };
const outage = new Error("503");

const listCases: [string, Parameters<typeof pairedListView>, object][] = [
	[
		"both lists loaded",
		[loaded([native()]), loaded([moonlight()])],
		{ loading: false, empty: false, nativeError: undefined },
	],
	[
		"a host without the native endpoint still lists its Moonlight devices",
		[failedWith(outage), loaded([moonlight()])],
		{ rows: [{ protocol: "moonlight" }], nativeError: outage, empty: false },
	],
	[
		"a failing Moonlight list keeps the native rows",
		[loaded([native()]), failedWith(outage)],
		{ rows: [{ protocol: "native" }], moonlightError: outage },
	],
	[
		"both failing is one outage, shown once",
		[failedWith(outage), failedWith(new Error("502"))],
		{ rows: [], nativeError: outage, moonlightError: undefined },
	],
	[
		"a failing refetch keeps the rows it had",
		[failedWith(outage, [native()]), loaded([])],
		{ rows: [{ protocol: "native" }], nativeError: outage },
	],
	[
		"no rows is empty only when no list failed",
		[loaded([]), loaded([])],
		{ rows: [], empty: true },
	],
	[
		"no rows beside a failing list is not empty",
		[loaded([]), failedWith(outage)],
		{ empty: false, moonlightError: outage },
	],
	["a list still loading", [loading, loaded([moonlight()])], { loading: true }],
];
for (const [name, args, want] of listCases) {
	test(`paired list: ${name}`, () => {
		expect(pairedListView(...args)).toMatchObject(want);
	});
}

test("access exists for a native row with any access field", () => {
	expect(hasAccess(nativeRow())).toBe(false);
	expect(hasAccess(nativeRow({ accessLevel: null, grants: null }))).toBe(false);
	expect(hasAccess(nativeRow({ accessLevel: "play" }))).toBe(true);
	expect(hasAccess(nativeRow({ grants: 0 }))).toBe(true);
	expect(hasAccess(nativeRow({ expiresUnix: 0 }))).toBe(true);
	// The disconnect flag alone does not make an old host's row editable.
	expect(hasAccess(nativeRow({ untilDisconnect: true }))).toBe(false);
	expect(hasAccess(moonlightRow({ accessLevel: "play" }))).toBe(false);
});

const controlCases: [string, PairedRow, boolean, object][] = [
	[
		"native with access, per-device policy",
		nativeRow({ accessLevel: "full" }),
		true,
		{ access: true, display: true, rename: false },
	],
	[
		"native with access, no per-device policy",
		nativeRow({ accessLevel: "full" }),
		false,
		{ access: true, display: false, rename: false },
	],
	[
		"native from an older host",
		nativeRow(),
		true,
		{ access: false, display: true, rename: false },
	],
	[
		"Moonlight",
		moonlightRow(),
		true,
		{ access: false, display: false, rename: true },
	],
];
for (const [name, row, perDevice, want] of controlCases) {
	test(`controls: ${name}`, () => {
		expect(rowControls(row, perDevice)).toEqual(want);
	});
}

test("stored access is the native row's, so a missing row counts as widening", () => {
	const rows = [
		moonlightRow({ fingerprint: "aa11" }),
		nativeRow({ grants: 3, expiresUnix: 50, untilDisconnect: true }),
	];
	expect(storedAccess(rows, "aa11")).toEqual({
		grants: 3,
		expires_unix: 50,
		until_disconnect: true,
	});
	expect(storedAccess(rows, "zz")).toBeUndefined();
	// A Moonlight row of the same fingerprint is not a native device.
	expect(storedAccess([moonlightRow({ fingerprint: "aa11" })], "aa11")).toBe(
		undefined,
	);
	expect(storedAccess([nativeRow()], "aa11")).toEqual({
		grants: undefined,
		expires_unix: undefined,
		until_disconnect: undefined,
	});
});

test("unpair-all calls only the planes that hold a device", () => {
	expect(planesWithRows([])).toEqual({ native: false, moonlight: false });
	expect(planesWithRows([nativeRow()])).toEqual({
		native: true,
		moonlight: false,
	});
	expect(planesWithRows([moonlightRow()])).toEqual({
		native: false,
		moonlight: true,
	});
	expect(planesWithRows([nativeRow(), moonlightRow()])).toEqual({
		native: true,
		moonlight: true,
	});
});

test("a device streams when a native session's client is its fingerprint's prefix", () => {
	const session = (plane: string, client: string) =>
		({ plane, client }) as SessionRow;
	expect(isStreaming([], "aa11")).toBe(false);
	expect(isStreaming([session("native", "aa")], "aa11")).toBe(true);
	expect(isStreaming([session("native", "bb")], "aa11")).toBe(false);
	expect(isStreaming([session("web", "aa")], "aa11")).toBe(true);
	expect(isStreaming([session("gamestream", "aa")], "aa11")).toBe(false);
});

test("a local row is no device's session, whatever its key spells", () => {
	const desk = { plane: "local", client: "desk" } as SessionRow;
	expect(isStreaming([desk], "desk-like")).toBe(false);
	expect(isStreaming([desk], "aa11")).toBe(false);
});

test("the pending fingerprint is the first running unpair that knows its device", () => {
	const idle = { isPending: false };
	const running = (fingerprint: string) => ({
		isPending: true,
		variables: { fingerprint },
	});
	expect(pendingFingerprint(idle, idle)).toBeNull();
	expect(pendingFingerprint(running("aa"), idle)).toBe("aa");
	expect(pendingFingerprint(idle, running("bb"))).toBe("bb");
	expect(pendingFingerprint(running("aa"), running("bb"))).toBe("aa");
	// A finished call keeps its last variables; they must not disable a row.
	expect(
		pendingFingerprint(
			{ isPending: false, variables: { fingerprint: "aa" } },
			idle,
		),
	).toBeNull();
	expect(pendingFingerprint({ isPending: true }, running("bb"))).toBe("bb");
});

test("renaming: cancel sends nothing, empty clears, text is trimmed", () => {
	expect(renameBody(null)).toBeNull();
	expect(renameBody("")).toEqual({ label: null });
	expect(renameBody("   ")).toEqual({ label: null });
	expect(renameBody("  Living room TV ")).toEqual({ label: "Living room TV" });
});

test("a nameless device's sheet is titled by the start of its fingerprint", () => {
	expect(deviceName(nativeRow({ name: "Deck" }))).toBe("Deck");
	expect(
		deviceName(moonlightRow({ name: "", fingerprint: "0123456789abcdef" })),
	).toBe("0123456789ab");
});
