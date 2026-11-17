// The pair swap is all-or-nothing, and the fallbacks are what keep legacy and custom-cert hosts
// serving. A native cert with the legacy key would be a console nobody can handshake with, so the
// mixed cases are the ones worth pinning down — including the Windows shape, which the resolver
// must get right without a win32 runtime to ask (see dirPrefix in tls-paths.mjs).
import { describe, expect, it } from "bun:test";
import { resolveUiTlsPaths } from "./tls-paths.mjs";

const DIR = "/home/you/.config/punktfunk";
const legacy = [`${DIR}/cert.pem`, `${DIR}/key.pem`] as const;
const native = [`${DIR}/native-cert.pem`, `${DIR}/native-key.pem`] as const;
/** `exists` over a fixed set of usable files on disk. */
const on =
	(...files: string[]) =>
	(p: string) =>
		files.includes(p);

describe("resolveUiTlsPaths", () => {
	it("prefers the native pair when both files are there", () => {
		expect(resolveUiTlsPaths(...legacy, on(...legacy, ...native))).toEqual({
			cert: native[0],
			key: native[1],
		});
	});

	it("keeps the legacy pair on a host that never took the identity split", () => {
		expect(resolveUiTlsPaths(...legacy, on(...legacy))).toEqual({
			cert: legacy[0],
			key: legacy[1],
		});
	});

	it("never mixes halves when only one native file is usable", () => {
		for (const half of native) {
			expect(resolveUiTlsPaths(...legacy, on(...legacy, half))).toEqual({
				cert: legacy[0],
				key: legacy[1],
			});
		}
	});

	// The Windows service supervisor hands us backslash paths (windows/service/web.rs);
	// node:path on a POSIX CI runner would read the whole thing as one filename and silently
	// never swap.
	it("resolves Windows paths without a win32 runtime", () => {
		const win = ["C:\\ProgramData\\punktfunk", "D:\\pf"] as const;
		for (const d of win) {
			expect(
				resolveUiTlsPaths(`${d}\\cert.pem`, `${d}\\key.pem`, () => true),
			).toEqual({
				cert: `${d}\\native-cert.pem`,
				key: `${d}\\native-key.pem`,
			});
		}
	});

	it("refuses to pair halves from two different directories", () => {
		expect(resolveUiTlsPaths("/a/cert.pem", "/b/key.pem", () => true)).toEqual({
			cert: "/a/cert.pem",
			key: "/b/key.pem",
		});
	});

	it("leaves the prefix verbatim rather than normalising it away", () => {
		// `join(dirname(p), …)` would collapse this to /a/native-cert.pem — a different directory
		// the moment `b` is a symlink.
		expect(
			resolveUiTlsPaths("/a/b/../cert.pem", "/a/b/../key.pem", () => true),
		).toEqual({
			cert: "/a/b/../native-cert.pem",
			key: "/a/b/../native-key.pem",
		});
	});

	it("leaves an operator's own cert alone, native pair present or not", () => {
		// Also covers the endsWith trap: "mycert.pem" ends with "cert.pem" but is not one.
		for (const own of [
			[`${DIR}/lan-ca.pem`, `${DIR}/lan-ca.key`],
			[`${DIR}/mycert.pem`, `${DIR}/mykey.pem`],
		] as const) {
			expect(resolveUiTlsPaths(...own, on(...own, ...native))).toEqual({
				cert: own[0],
				key: own[1],
			});
		}
	});

	it("does not re-swap a pair that already names the native files", () => {
		expect(resolveUiTlsPaths(...native, () => true)).toEqual({
			cert: native[0],
			key: native[1],
		});
	});

	it("passes a half-configured pair through for the entry to refuse", () => {
		expect(resolveUiTlsPaths(legacy[0], undefined, on(...native))).toEqual({
			cert: legacy[0],
			key: undefined,
		});
		expect(resolveUiTlsPaths(undefined, undefined, on(...native))).toEqual({
			cert: undefined,
			key: undefined,
		});
	});
});
