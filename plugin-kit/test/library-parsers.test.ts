// The parser ports, tested against the SAME cases the host's Rust scanners pin.
//
// These are not "does TypeScript work" tests. The formats here are undocumented and the host's
// versions are the reference implementation; a port that drifts produces a library that looks fine
// and launches nothing. Where a Rust test exists, its assertions are carried over verbatim — the
// per-plugin parity harness (design M5) then checks the whole pipeline against a live host, but
// these catch a drift long before that.

import { Database } from "bun:sqlite";
import { describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import {
	confinedJoin,
	crc32,
	dirAccess,
	fileAccess,
	fileUrl,
	findGridArtFile,
	findLocalArtFile,
	gridFilenames,
	isSteamTool,
	openReadOnly,
	parseAppManifest,
	parseRegQuery,
	parseRegSubKeys,
	parseShortcuts,
	readTextCapped,
	shortcutAppId,
	shortcutGameId,
	spawnAgainIfKilled,
	steamCdnUrl,
	steamLibraryDirs,
	steamListedLibraries,
	vdfPaths,
	vdfValue,
	withReadOnlyDb,
} from "../src/library/parsers/index.js";

const tmp = (name: string): string => {
	const dir = path.join(os.tmpdir(), `pf-kit-${name}-${process.pid}`);
	fs.mkdirSync(dir, { recursive: true });
	return dir;
};

describe("text VDF / ACF", () => {
	test("vdfValue extracts a quoted field", () => {
		expect(vdfValue('"path"\t\t"/mnt/games/SteamLibrary"', "path")).toBe(
			"/mnt/games/SteamLibrary",
		);
		expect(vdfValue('"appid"\t\t"570"', "appid")).toBe("570");
		expect(vdfValue('"name"\t\t"Dota 2"', "name")).toBe("Dota 2");
		// Wrong key → nothing (a prefix match must not leak the neighbouring field).
		expect(vdfValue('"installdir"\t\t"x"', "appid")).toBeUndefined();
	});

	test("vdfPaths pulls every library folder and unescapes Windows separators", () => {
		const vdf = `
"libraryfolders"
{
	"0"
	{
		"path"		"/home/u/.local/share/Steam"
		"label"		""
	}
	"1"
	{
		"path"		"D:\\\\SteamLibrary"
	}
}`;
		expect(vdfPaths(vdf)).toEqual([
			"/home/u/.local/share/Steam",
			"D:\\SteamLibrary",
		]);
	});

	test("listed Steam libraries keep an unbound path for the host to judge", () => {
		const root = tmp("steam-listed");
		const steamapps = path.join(root, "steamapps");
		fs.mkdirSync(steamapps, { recursive: true });
		const missing = path.join(root, "other-drive");
		fs.writeFileSync(
			path.join(steamapps, "libraryfolders.vdf"),
			`"libraryfolders"\n{\n\t"1"\n\t{\n\t\t"path" "${missing}"\n\t}\n}`,
		);
		expect(steamListedLibraries([root])).toEqual([
			path.resolve(missing),
			path.resolve(root),
		]);
		expect(steamLibraryDirs([root])).toEqual([path.resolve(steamapps)]);
	});

	test("parseAppManifest reads the flat fields it needs", () => {
		const acf = `"AppState"
{
	"appid"		"570"
	"name"		"Dota 2"
	"installdir"		"dota 2 beta"
}`;
		expect(parseAppManifest(acf)).toEqual({
			appid: 570,
			name: "Dota 2",
			installdir: "dota 2 beta",
		});
		// A manifest missing the essentials is not a title.
		expect(parseAppManifest('"AppState" { "name" "x" }')).toBeUndefined();
	});

	test("isSteamTool keeps runtimes out of a game library", () => {
		expect(isSteamTool(228980, "Steamworks Common Redistributables")).toBe(
			true,
		);
		expect(isSteamTool(1628350, "Steam Linux Runtime 3.0 (sniper)")).toBe(true);
		expect(isSteamTool(999, "Proton 9.0")).toBe(true);
		expect(isSteamTool(999, "SteamVR")).toBe(true);
		expect(isSteamTool(570, "Dota 2")).toBe(false);
	});
});

describe("binary shortcuts.vdf", () => {
	/** Build a binary shortcuts.vdf the way Steam writes one. */
	const buildShortcuts = (
		entries: ReadonlyArray<{
			appid?: number;
			appname: string;
			exe: string;
			hidden?: boolean;
		}>,
	): Uint8Array => {
		const parts: number[] = [];
		const cstr = (s: string) => {
			for (const b of new TextEncoder().encode(s)) parts.push(b);
			parts.push(0);
		};
		const i32 = (v: number) => {
			parts.push(
				v & 0xff,
				(v >>> 8) & 0xff,
				(v >>> 16) & 0xff,
				(v >>> 24) & 0xff,
			);
		};
		parts.push(0x00);
		cstr("shortcuts");
		entries.forEach((e, i) => {
			parts.push(0x00);
			cstr(String(i));
			if (e.appid !== undefined) {
				parts.push(0x02);
				cstr("appid");
				i32(e.appid);
			}
			parts.push(0x01);
			cstr("AppName");
			cstr(e.appname);
			parts.push(0x01);
			cstr("Exe");
			cstr(e.exe);
			parts.push(0x02);
			cstr("IsHidden");
			i32(e.hidden ? 1 : 0);
			// A nested map the parser must skip wholesale.
			parts.push(0x00);
			cstr("tags");
			parts.push(0x01);
			cstr("0");
			cstr("favourite");
			parts.push(0x08);
			parts.push(0x08); // end of this shortcut
		});
		parts.push(0x08); // end of shortcuts
		parts.push(0x08); // end of document
		return new Uint8Array(parts);
	};

	test("parses entries, skips nested maps, and reads the hidden flag", () => {
		const buf = buildShortcuts([
			{ appid: 2456789012, appname: "My Emulator", exe: '"/usr/bin/foo"' },
			{ appid: 3000000000, appname: "Hidden One", exe: '"/x"', hidden: true },
		]);
		const got = parseShortcuts(buf);
		expect(got).toHaveLength(2);
		expect(got[0]).toMatchObject({
			appid: 2456789012,
			name: "My Emulator",
			hidden: false,
		});
		expect(got[1]).toMatchObject({ name: "Hidden One", hidden: true });
	});

	test("derives the appid when the file omits it", () => {
		const buf = buildShortcuts([{ appname: "No Appid", exe: '"/usr/bin/x"' }]);
		const got = parseShortcuts(buf);
		expect(got).toHaveLength(1);
		// Derived ids always carry the high bit — that is how a shortcut is told apart from a real
		// store appid downstream (and why its CDN art fetch is skipped).
		expect(got[0].appid & 0x8000_0000).not.toBe(0);
		expect(got[0].appid).toBe(shortcutAppId('"/usr/bin/x"', "No Appid"));
	});

	test("is total on a truncated or garbled file", () => {
		expect(parseShortcuts(new Uint8Array([]))).toEqual([]);
		expect(parseShortcuts(new Uint8Array([0x01, 0x02, 0x03]))).toEqual([]);
		const good = buildShortcuts([{ appid: 1, appname: "A", exe: "/a" }]);
		// Every truncation of a valid file must return, not throw.
		for (let i = 0; i < good.length; i++) {
			expect(() => parseShortcuts(good.subarray(0, i))).not.toThrow();
		}
	});

	test("crc32 matches the IEEE check value", () => {
		// The canonical CRC-32 check: crc32("123456789") == 0xCBF43926.
		expect(crc32(new TextEncoder().encode("123456789"))).toBe(0xcbf4_3926);
	});

	test("shortcutGameId composes the appid and the shortcut marker", () => {
		// high dword = appid, low dword = 0x02000000. Handing rungameid the bare 32-bit appid does
		// NOT launch a shortcut, which is the entire reason this function exists.
		const id = BigInt(shortcutGameId(0x8000_0000));
		expect(id >> 32n).toBe(0x8000_0000n);
		expect(id & 0xffff_ffffn).toBe(0x0200_0000n);
		// Digits only — it rides the `steam_appid` launch kind, which the host validates as digits.
		expect(shortcutGameId(2_456_789_012)).toMatch(/^\d+$/);
	});
});

describe("path confinement", () => {
	test("confinedJoin refuses anything that could escape the install dir", () => {
		const base = path.join(path.sep, "games", "W3");
		expect(confinedJoin(base, "bin/game.exe")).toBe(
			path.join(base, "bin", "game.exe"),
		);
		expect(confinedJoin(base, "bin\\game.exe")).toBe(
			path.join(base, "bin", "game.exe"),
		);
		// The three shapes a crafted goggame-*.info would use to point elsewhere.
		expect(
			confinedJoin(base, "../../windows/system32/cmd.exe"),
		).toBeUndefined();
		expect(confinedJoin(base, "/etc/passwd")).toBeUndefined();
		expect(
			confinedJoin(base, "C:\\Windows\\system32\\cmd.exe"),
		).toBeUndefined();
		expect(confinedJoin(base, "")).toBeUndefined();
	});
});

describe("capped reads", () => {
	test("readTextCapped refuses an over-cap file and a missing one", () => {
		const dir = tmp("caps");
		const small = path.join(dir, "small.txt");
		fs.writeFileSync(small, "hello");
		expect(readTextCapped(small)).toBe("hello");
		expect(readTextCapped(small, 2)).toBeUndefined(); // over the cap
		expect(readTextCapped(path.join(dir, "nope.txt"))).toBeUndefined();
		expect(readTextCapped(dir)).toBeUndefined(); // a directory is not a file
		fs.rmSync(dir, { recursive: true, force: true });
	});
});

describe("art locations", () => {
	test("steamCdnUrl skips shortcut appids, which have no CDN entry", () => {
		expect(steamCdnUrl(570, "header")).toContain("/570/header.jpg");
		expect(steamCdnUrl(570, "portrait")).toContain("library_600x900.jpg");
		// The local cache names the header asset differently from the CDN — pinned because it is
		// the single most common way to get Steam art wrong.
		expect(steamCdnUrl(570, "header")).not.toContain("library_header");
		expect(steamCdnUrl(0x8000_0001, "header")).toBeUndefined();
	});

	test("grid filenames follow Steam's per-kind naming", () => {
		expect(gridFilenames(570, "portrait")).toEqual(["570p.png", "570p.jpg"]);
		expect(gridFilenames(570, "hero")).toEqual([
			"570_hero.png",
			"570_hero.jpg",
		]);
		expect(gridFilenames(570, "logo")).toEqual([
			"570_logo.png",
			"570_logo.jpg",
		]);
		expect(gridFilenames(570, "header")).toEqual(["570.png", "570.jpg"]);
	});

	test("finds cached and user-override art on disk", () => {
		const dir = tmp("art");
		const hashDir = path.join(dir, "appcache", "librarycache", "570", "abc123");
		fs.mkdirSync(hashDir, { recursive: true });
		fs.writeFileSync(path.join(hashDir, "library_600x900.jpg"), "x");
		expect(findLocalArtFile(dir, 570, "portrait")).toBe(
			path.join(hashDir, "library_600x900.jpg"),
		);
		expect(findLocalArtFile(dir, 570, "hero")).toBeUndefined();

		const cfg = path.join(dir, "userdata", "1", "config");
		fs.mkdirSync(path.join(cfg, "grid"), { recursive: true });
		fs.writeFileSync(path.join(cfg, "grid", "570p.jpg"), "x");
		expect(findGridArtFile(cfg, 570, "portrait")).toBe(
			path.join(cfg, "grid", "570p.jpg"),
		);
		fs.rmSync(dir, { recursive: true, force: true });
	});

	test("finds a cover cached under Steam's newer `library_capsule` name", () => {
		// The bug this pins: appid 2483190 (Forza Horizon 6) caches its 300×450 cover as
		// `library_capsule.jpg`, the flat CDN URL for its `library_600x900.jpg` 404s, and the client
		// therefore fell through to the header and drew a banner in a 2:3 poster slot.
		const dir = tmp("art-capsule");
		const hashDir = path.join(
			dir,
			"appcache",
			"librarycache",
			"2483190",
			"711e",
		);
		fs.mkdirSync(hashDir, { recursive: true });
		fs.writeFileSync(path.join(hashDir, "library_capsule.jpg"), "x");
		expect(findLocalArtFile(dir, 2483190, "portrait")).toBe(
			path.join(hashDir, "library_capsule.jpg"),
		);
		fs.rmSync(dir, { recursive: true, force: true });
	});

	test("finds art stored straight in the appid dir, with no hash dir", () => {
		// The majority layout — 623 of 779 appids on the reference cache. A hash-dir-only walk finds
		// none of it and silently falls back to a CDN URL that 404s for anything re-hashed.
		const dir = tmp("art-flat");
		const appDir = path.join(dir, "appcache", "librarycache", "813230");
		fs.mkdirSync(appDir, { recursive: true });
		fs.writeFileSync(path.join(appDir, "library_600x900.jpg"), "x");
		fs.writeFileSync(path.join(appDir, "header.jpg"), "x");
		expect(findLocalArtFile(dir, 813230, "portrait")).toBe(
			path.join(appDir, "library_600x900.jpg"),
		);
		// `header.jpg` is the CDN's name, but the local cache uses it too for newer entries — the
		// two spellings are the same 460×215 asset and never appear together for one appid.
		expect(findLocalArtFile(dir, 813230, "header")).toBe(
			path.join(appDir, "header.jpg"),
		);
		expect(findLocalArtFile(dir, 813230, "hero")).toBeUndefined();
		fs.rmSync(dir, { recursive: true, force: true });
	});

	test("a hash dir wins over a loose file of the same kind", () => {
		// No appid on the reference cache carries both, so this is only about which copy is the
		// current one if Steam ever leaves the old layout behind: the hash dir is what it re-fetches
		// into, so that is the copy it is itself displaying.
		const dir = tmp("art-both");
		const appDir = path.join(dir, "appcache", "librarycache", "570");
		const hashDir = path.join(appDir, "abc123");
		fs.mkdirSync(hashDir, { recursive: true });
		fs.writeFileSync(path.join(appDir, "library_600x900.jpg"), "x");
		fs.writeFileSync(path.join(hashDir, "library_600x900.jpg"), "x");
		expect(findLocalArtFile(dir, 570, "portrait")).toBe(
			path.join(hashDir, "library_600x900.jpg"),
		);
		fs.rmSync(dir, { recursive: true, force: true });
	});

	test("fileUrl produces the host's local-art contract shape", () => {
		const u = fileUrl(path.join(path.sep, "home", "u", "My Games", "c.jpg"));
		expect(u.startsWith("file:///")).toBe(true);
		// Spaces are percent-encoded; the separators survive so the host can rebuild the path.
		expect(u).toContain("My%20Games");
		expect(u).toContain("/c.jpg");
	});
});

describe("reg.exe output", () => {
	test("parses value rows and leaves the key header alone", () => {
		const stdout = [
			"",
			"HKEY_LOCAL_MACHINE\\SOFTWARE\\WOW6432Node\\Valve\\Steam",
			"    InstallPath    REG_SZ    C:\\Program Files (x86)\\Steam",
			"    Language    REG_SZ    english",
			"",
		].join("\r\n");
		expect(parseRegQuery(stdout)).toEqual([
			{
				name: "InstallPath",
				type: "REG_SZ",
				// Data may contain spaces — only the first two columns are split off.
				data: "C:\\Program Files (x86)\\Steam",
			},
			{ name: "Language", type: "REG_SZ", data: "english" },
		]);
	});

	test("a killed spawn runs once more; an exit of any code is final", () => {
		// `status: null` is a kill (the misfired timeout). Exit 1 is reg.exe's "no such key".
		const scripted = (...statuses: (number | null)[]) => {
			const seen: (number | null)[] = [];
			const last = spawnAgainIfKilled(() => {
				const status = statuses[seen.length] ?? null;
				seen.push(status);
				return { status };
			});
			return { status: last.status, spawns: seen.length };
		};
		expect(scripted(null, 0)).toEqual({ status: 0, spawns: 2 });
		expect(scripted(1)).toEqual({ status: 1, spawns: 1 });
		// A real hang is killed twice and still reads as failed: bounded, never a loop.
		expect(scripted(null, null, 0)).toEqual({ status: null, spawns: 2 });
	});
});

// The read-only SQLite helper, against a REAL database file.
//
// This exists because its absence shipped a total failure. `openReadOnly` built a
// `file:…?immutable=1` URI but opened it with `{ readonly: true }`, which does not enable SQLite's
// URI filename parsing — so the name was taken literally, the open threw, and `openReadOnly`
// returned `undefined`. Every caller reads that as "this launcher isn't installed", and
// `withReadOnlyDb(...) ?? []` turns it into an empty library. The lutris plugin therefore reported
// "0 games" on every box, forever, while `detect` still said "present" (it only stats the file) —
// and the only thing that caught it was a hand-run parity gate against a live host.
//
// So: assert the helper can actually READ, not merely that it returns something.
describe("openReadOnly", () => {
	const withDb = <T>(use: (file: string) => T): T => {
		const dir = fs.mkdtempSync(path.join(os.tmpdir(), "pf-kit-sqlite-"));
		const file = path.join(dir, "pga.db");
		const seed = new Database(file);
		seed.run(
			"CREATE TABLE games (id INTEGER PRIMARY KEY, name TEXT, installed INT)",
		);
		seed.run(
			"INSERT INTO games (id, name, installed) VALUES (1, 'Ubisoft Connect', 1)",
		);
		seed.close();
		try {
			return use(file);
		} finally {
			fs.rmSync(dir, { recursive: true, force: true });
		}
	};

	// The first bun:sqlite open in this file loads the native module, and on a starved CI runner
	// that alone passes bun's 5 s default.
	test("opens a real database and returns its rows", () => {
		withDb((file) => {
			const db = openReadOnly(file);
			expect(db).toBeDefined();
			expect(
				db?.query("SELECT id, name FROM games WHERE installed = 1"),
			).toEqual([{ id: 1, name: "Ubisoft Connect" }]);
			db?.close();
		});
	}, 30_000);

	// A path with a space is the realistic URI-encoding case (Flatpak roots, "Program Files").
	test("opens a path that needs URI escaping", () => {
		const dir = fs.mkdtempSync(path.join(os.tmpdir(), "pf kit sqlite "));
		const file = path.join(dir, "pga.db");
		const seed = new Database(file);
		seed.run("CREATE TABLE games (id INTEGER PRIMARY KEY)");
		seed.run("INSERT INTO games (id) VALUES (7)");
		seed.close();
		try {
			expect(openReadOnly(file)?.query("SELECT id FROM games")).toEqual([
				{ id: 7 },
			]);
		} finally {
			fs.rmSync(dir, { recursive: true, force: true });
		}
	});

	test("withReadOnlyDb reads, then closes", () => {
		withDb((file) => {
			expect(
				withReadOnlyDb(file, (h) => h.query("SELECT name FROM games")),
			).toEqual([{ name: "Ubisoft Connect" }]);
		});
	});

	// The "not installed" contract — an absent file is `undefined`, never a throw.
	test("absent file is undefined, not an error", () => {
		expect(
			openReadOnly(path.join(os.tmpdir(), "pf-kit-nope", "pga.db")),
		).toBeUndefined();
		expect(
			withReadOnlyDb(path.join(os.tmpdir(), "pf-kit-nope", "pga.db"), () => 1),
		).toBeUndefined();
	});

	// Schema drift degrades to no rows rather than taking the plugin down.
	test("a bad query returns [] rather than throwing", () => {
		withDb((file) => {
			const db = openReadOnly(file);
			expect(db?.query("SELECT missing_column FROM games")).toEqual([]);
			db?.close();
		});
	});
});

// Subkey enumeration, against the output reg.exe ACTUALLY prints.
//
// This had no coverage and was broken end to end: it matched lines against the abbreviated
// `HKLM\…` prefix it was handed, but reg.exe echoes `HKEY_LOCAL_MACHINE\…`. Nothing ever matched,
// so it returned [] on every machine, and the GOG plugin — its only consumer — reported "no games
// installed" rather than failing. Caught on hardware by the parity gate: the host's built-in
// scanner found IRON NEST, the plugin found nothing.
//
// The fixture is the verbatim output from .173 (a blank line, then one subkey row).
describe("parseRegSubKeys", () => {
	const KEY = "HKLM\\SOFTWARE\\WOW6432Node\\GOG.com\\Games";

	test("returns subkey NAMES from real reg.exe output", () => {
		const stdout = [
			"",
			"HKEY_LOCAL_MACHINE\\SOFTWARE\\WOW6432Node\\GOG.com\\Games\\2013434102",
			"",
		].join("\r\n");
		// The name is the GOG product id, and the consumer composes `${KEY}\\${name}`.
		expect(parseRegSubKeys(stdout, KEY)).toEqual(["2013434102"]);
	});

	test("several subkeys, in order", () => {
		const base = "HKEY_LOCAL_MACHINE\\SOFTWARE\\WOW6432Node\\GOG.com\\Games";
		const stdout = ["", `${base}\\1207658930`, `${base}\\2013434102`].join(
			"\r\n",
		);
		expect(parseRegSubKeys(stdout, KEY)).toEqual(["1207658930", "2013434102"]);
	});

	// reg.exe /s output nests deeper; only immediate children are subkeys of this key.
	test("ignores grandchildren", () => {
		const base = "HKEY_LOCAL_MACHINE\\SOFTWARE\\WOW6432Node\\GOG.com\\Games";
		const stdout = [
			"",
			`${base}\\2013434102`,
			`${base}\\2013434102\\tasks`,
		].join("\r\n");
		expect(parseRegSubKeys(stdout, KEY)).toEqual(["2013434102"]);
	});

	// The queried key itself is echoed as a header when it has values; it is not its own subkey.
	test("does not return the queried key itself", () => {
		const stdout = [
			"",
			"HKEY_LOCAL_MACHINE\\SOFTWARE\\WOW6432Node\\GOG.com\\Games",
			"",
		].join("\r\n");
		expect(parseRegSubKeys(stdout, KEY)).toEqual([]);
	});

	test("case-insensitive on the hive and path", () => {
		const stdout =
			"hkey_local_machine\\software\\wow6432node\\gog.com\\games\\42";
		expect(parseRegSubKeys(stdout, KEY)).toEqual(["42"]);
	});

	test("no subkeys is empty, not a throw", () => {
		expect(parseRegSubKeys("", KEY)).toEqual([]);
		expect(
			parseRegSubKeys("ERROR: The system was unable to find...", KEY),
		).toEqual([]);
	});
});

describe("access classification", () => {
	// The distinction the Windows runner depends on: a launcher inside a user profile stats
	// exactly like an absent one unless EACCES is told apart from ENOENT. Reproduced with a
	// directory stripped of traverse permission, which is the same errno everywhere.
	const root = fs.mkdtempSync(path.join(os.tmpdir(), "pf-access-"));

	test("present, absent and unreadable are three different answers", () => {
		const open = path.join(root, "open");
		fs.mkdirSync(open);
		fs.writeFileSync(path.join(open, "emu.exe"), "x");

		expect(fileAccess(path.join(open, "emu.exe"))).toBe("ok");
		expect(dirAccess(open)).toBe("ok");
		expect(fileAccess(path.join(open, "nope.exe"))).toBe("missing");
		expect(dirAccess(path.join(root, "nope"))).toBe("missing");

		// An empty file is not a usable manifest, and that is "missing", never "denied".
		fs.writeFileSync(path.join(open, "empty.exe"), "");
		expect(fileAccess(path.join(open, "empty.exe"))).toBe("missing");

		// Root ignores the mode bits, so it cannot observe a denial.
		if (process.platform === "win32" || process.getuid?.() === 0) return;
		const shut = path.join(root, "shut");
		fs.mkdirSync(shut);
		fs.writeFileSync(path.join(shut, "emu.exe"), "x");
		fs.chmodSync(shut, 0o000);
		try {
			expect(fileAccess(path.join(shut, "emu.exe"))).toBe("denied");
		} finally {
			fs.chmodSync(shut, 0o700);
		}
	});
});

describe.skipIf(process.platform !== "linux")("steamRoots", () => {
	test("one Steam install is one root, whichever link reaches it", () => {
		const home = fs.mkdtempSync(path.join(os.tmpdir(), "pf-steam-home-"));
		try {
			const real = path.join(home, ".local", "share", "Steam");
			fs.mkdirSync(path.join(real, "steamapps"), { recursive: true });
			fs.mkdirSync(path.join(home, ".steam"));
			fs.symlinkSync(real, path.join(home, ".steam", "steam"));
			fs.symlinkSync(real, path.join(home, ".steam", "root"));
			// A child with that HOME: this runtime's os.homedir() ignores a changed process.env.
			const parsers = path.join(
				import.meta.dir,
				"..",
				"src",
				"library",
				"parsers",
				"index.ts",
			);
			const child = Bun.spawnSync(
				[
					process.execPath,
					"-e",
					`const { steamRoots } = await import(${JSON.stringify(parsers)}); console.log(JSON.stringify(steamRoots()));`,
				],
				{ env: { ...process.env, HOME: home } },
			);
			expect(JSON.parse(child.stdout.toString())).toEqual([
				fs.realpathSync(real),
			]);
		} finally {
			fs.rmSync(home, { recursive: true, force: true });
		}
	});
});
