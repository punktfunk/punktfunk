// `reconcileSharedSdk` runs on EVERY runner start, so its no-op path is the safety-critical one:
// a false positive deletes a working lockfile and re-resolves the whole tree on a box that was
// fine. These tests pin the decision, not the install (which needs a registry).
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { afterEach, describe, expect, test } from "bun:test";
import { installedSdkVersion, movedPins, reconcileSharedSdk } from "../src/plugins.js";
import { SDK_VERSION } from "../src/version.js";

const dirs: string[] = [];

/** A plugins tree whose installed `@punktfunk/host` is `version` (omit for "not installed"). */
const tree = (version?: string): string => {
	const dir = fs.mkdtempSync(path.join(os.tmpdir(), "pf-reconcile-"));
	dirs.push(dir);
	fs.writeFileSync(path.join(dir, "package.json"), '{"private":true}\n');
	fs.writeFileSync(path.join(dir, "bun.lock"), "ORIGINAL-LOCK\n");
	if (version !== undefined) {
		const host = path.join(dir, "node_modules", "@punktfunk", "host");
		fs.mkdirSync(host, { recursive: true });
		fs.writeFileSync(
			path.join(host, "package.json"),
			JSON.stringify({ name: "@punktfunk/host", version }),
		);
	}
	return dir;
};

afterEach(() => {
	for (const d of dirs.splice(0)) fs.rmSync(d, { recursive: true, force: true });
});

describe("installedSdkVersion", () => {
	test("reads the installed version, and is undefined when absent", () => {
		expect(installedSdkVersion(tree("0.1.2"))).toBe("0.1.2");
		expect(installedSdkVersion(tree())).toBeUndefined();
	});

	test("is undefined rather than throwing on a corrupt manifest", () => {
		const dir = tree("0.1.2");
		fs.writeFileSync(
			path.join(dir, "node_modules", "@punktfunk", "host", "package.json"),
			"{ not json",
		);
		expect(installedSdkVersion(dir)).toBeUndefined();
	});
});

describe("reconcileSharedSdk", () => {
	// The common case, every start, on every healthy box: touch nothing.
	test("is a silent no-op when the installed SDK already matches", () => {
		const dir = tree(SDK_VERSION);
		const lines: string[] = [];
		reconcileSharedSdk(dir, (l) => lines.push(l));
		expect(fs.readFileSync(path.join(dir, "bun.lock"), "utf8")).toBe(
			"ORIGINAL-LOCK\n",
		);
		expect(lines).toEqual([]);
	});

	// A tree with no SDK has no plugins yet — the first `bun add` resolves the current one, so
	// there is nothing to refresh and nothing to log about.
	test("is a silent no-op when no SDK is installed at all", () => {
		const dir = tree();
		const lines: string[] = [];
		reconcileSharedSdk(dir, (l) => lines.push(l));
		expect(fs.readFileSync(path.join(dir, "bun.lock"), "utf8")).toBe(
			"ORIGINAL-LOCK\n",
		);
		expect(lines).toEqual([]);
	});

	// The failure path matters as much as the happy one: this runs unattended at boot, and the
	// tree it just took the lockfile away from is the one the operator's plugins load from. The
	// install cannot succeed here (the fake package.json resolves nothing), so this exercises the
	// real rollback.
	test("restores the lockfile and the packages and keeps going when the refresh fails", () => {
		const dir = tree("0.0.1-not-a-real-version");
		// A plugin's own file, which the failed install must leave exactly as it was.
		const plugin = path.join(dir, "node_modules", "punktfunk-plugin-x", "index.js");
		fs.mkdirSync(path.dirname(plugin), { recursive: true });
		fs.writeFileSync(plugin, "the bytes that ran before");
		const lines: string[] = [];
		expect(() => reconcileSharedSdk(dir, (l) => lines.push(l))).not.toThrow();
		expect(fs.readFileSync(path.join(dir, "bun.lock"), "utf8")).toBe(
			"ORIGINAL-LOCK\n",
		);
		expect(fs.readFileSync(plugin, "utf8")).toBe("the bytes that ran before");
		expect(installedSdkVersion(dir)).toBe("0.0.1-not-a-real-version");
		expect(fs.existsSync(path.join(dir, "node_modules.pf-bak"))).toBe(false);
		expect(lines.join("\n")).toContain("WARNING");
		// And it names both versions, so the log says what it was trying to do.
		expect(lines.join("\n")).toContain("0.0.1-not-a-real-version");
		expect(fs.existsSync(path.join(dir, "bun.lock.pf-bak"))).toBe(false);
	});
});

describe("movedPins", () => {
	// The shape bun 1.4.4 writes. A lockless re-resolve keeps the installed bytes of a plugin at
	// the same version, but records whatever integrity the registry serves now.
	const lock = (sdk: string, plugin: string) => `{
  "packages": {
    "@t/plugin-x": ["@t/plugin-x@1.0.0", "https://r.example/x.tgz", { "dependencies": { "@punktfunk/host": "^0.1.0" } }, "${plugin}"],

    "@punktfunk/host": ["@punktfunk/host@${sdk}", "", {}, "sha512-sdk-${sdk}"],
  }
}`;

	test("refuses a plugin republished under its pinned version, never the floating SDK", () => {
		const dir = tree("0.1.0");
		fs.writeFileSync(
			path.join(dir, "package.json"),
			JSON.stringify({ dependencies: { "@t/plugin-x": "1.0.0" } }),
		);
		const before = lock("0.1.0", "sha512-reviewed");
		expect(movedPins(dir, before, lock("0.2.0", "sha512-reviewed"))).toEqual([]);
		expect(movedPins(dir, before, lock("0.2.0", "sha512-republished"))).toEqual(["@t/plugin-x"]);
		expect(movedPins(dir, before, "{}")).toEqual(["@t/plugin-x"]);
		// A tree with no lock had no pin to hold.
		expect(movedPins(dir, "", lock("0.2.0", "sha512-republished"))).toEqual([]);
	});
});
