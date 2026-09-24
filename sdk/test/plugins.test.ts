// The `punktfunk-host plugins …` package-op helpers: friendly-name resolution, the bunfig scope
// wiring (idempotent + merges into an existing file), and installed-plugin discovery. `add`/`remove`
// shell out to bun over the network, so the live install is verified on-glass, not here.
import { afterAll, describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as path from "node:path";
import {
	ensureBunfig,
	ensurePluginsDir,
	listInstalled,
	REGISTRY,
	reconcileSharedSdk,
	resolvePackage,
	stripGroupWrite,
} from "../src/plugins.js";

const ROOT = path.join(import.meta.dir, "..", `.plugins-fixtures-${process.pid}`);
fs.mkdirSync(ROOT, { recursive: true });
afterAll(() => fs.rmSync(ROOT, { recursive: true, force: true }));

const tmp = (name: string): string => {
	const dir = path.join(ROOT, name);
	fs.mkdirSync(dir, { recursive: true });
	return dir;
};

const writePkg = (dir: string, name: string, version: string) => {
	const pkgDir = path.join(dir, "node_modules", name);
	fs.mkdirSync(pkgDir, { recursive: true });
	fs.writeFileSync(
		path.join(pkgDir, "package.json"),
		JSON.stringify({ name, version }),
	);
};

describe("reconcileSharedSdk", () => {
	test("a refresh that failed is not repeated on the next start", () => {
		const dir = tmp("sdk-refresh");
		writePkg(dir, "@punktfunk/host", "0.0.1");
		// A dependency bun can't resolve offline makes the install fail fast.
		fs.writeFileSync(
			path.join(dir, "package.json"),
			JSON.stringify({ dependencies: { missing: "file:./not-there" } }),
		);
		const lines: string[] = [];
		reconcileSharedSdk(dir, (l) => lines.push(l));
		expect(lines.some((l) => l.includes("WARNING"))).toBe(true);
		lines.length = 0;
		reconcileSharedSdk(dir, (l) => lines.push(l));
		expect(lines).toEqual([]);
		// A changed plugin set retries.
		fs.writeFileSync(
			path.join(dir, "package.json"),
			JSON.stringify({ dependencies: { missing: "file:./still-not-there" } }),
		);
		reconcileSharedSdk(dir, (l) => lines.push(l));
		expect(lines.some((l) => l.includes("refreshing"))).toBe(true);
	});
});

describe("resolvePackage", () => {
	test("maps bare first-party names into the @punktfunk scope", () => {
		expect(resolvePackage("playnite")).toBe("@punktfunk/plugin-playnite");
		expect(resolvePackage("rom-manager")).toBe("@punktfunk/plugin-rom-manager");
	});

	test("passes @punktfunk-scoped names through verbatim (our registry, no gate)", () => {
		expect(resolvePackage("@punktfunk/plugin-playnite")).toBe(
			"@punktfunk/plugin-playnite",
		);
	});

	test("refuses public-registry names without allowPublicRegistry", () => {
		expect(() => resolvePackage("punktfunk-plugin-custom")).toThrow(
			/public/i,
		);
		expect(() => resolvePackage("@someone/plugin-x")).toThrow(/public/i);
		expect(() => resolvePackage("some/registry-path")).toThrow(/public/i);
	});

	test("passes public-registry names through with allowPublicRegistry", () => {
		const allow = { allowPublicRegistry: true };
		expect(resolvePackage("punktfunk-plugin-custom", allow)).toBe(
			"punktfunk-plugin-custom",
		);
		expect(resolvePackage("@someone/plugin-x", allow)).toBe(
			"@someone/plugin-x",
		);
	});

	test("trims and rejects empty", () => {
		expect(resolvePackage("  playnite  ")).toBe("@punktfunk/plugin-playnite");
		expect(() => resolvePackage("   ")).toThrow();
	});
});

describe("ensureBunfig", () => {
	test("writes the @punktfunk scope map when absent", () => {
		const dir = tmp("bunfig-fresh");
		ensureBunfig(dir);
		const toml = fs.readFileSync(path.join(dir, "bunfig.toml"), "utf8");
		expect(toml).toContain("[install.scopes]");
		expect(toml).toContain(`"@punktfunk" = "${REGISTRY}"`);
	});

	test("is idempotent — a second call doesn't duplicate the scope", () => {
		const dir = tmp("bunfig-idempotent");
		ensureBunfig(dir);
		ensureBunfig(dir);
		const toml = fs.readFileSync(path.join(dir, "bunfig.toml"), "utf8");
		expect(toml.match(/@punktfunk/g)?.length).toBe(1);
	});

	test("merges into an existing [install.scopes] table, keeping other scopes", () => {
		const dir = tmp("bunfig-merge");
		fs.writeFileSync(
			path.join(dir, "bunfig.toml"),
			'[install.scopes]\n"@other" = "https://example.test/npm/"\n',
		);
		ensureBunfig(dir);
		const toml = fs.readFileSync(path.join(dir, "bunfig.toml"), "utf8");
		expect(toml).toContain('"@other" = "https://example.test/npm/"');
		expect(toml).toContain(`"@punktfunk" = "${REGISTRY}"`);
		expect(toml.match(/\[install\.scopes\]/g)?.length).toBe(1);
	});

	test("appends a table to an unrelated existing bunfig", () => {
		const dir = tmp("bunfig-append");
		fs.writeFileSync(path.join(dir, "bunfig.toml"), "telemetry = false\n");
		ensureBunfig(dir);
		const toml = fs.readFileSync(path.join(dir, "bunfig.toml"), "utf8");
		expect(toml).toContain("telemetry = false");
		expect(toml).toContain("[install.scopes]");
		expect(toml).toContain(`"@punktfunk" = "${REGISTRY}"`);
	});
});

describe("listInstalled", () => {
	test("returns empty for a dir with no node_modules", () => {
		expect(listInstalled(tmp("list-empty"))).toEqual([]);
	});

	test("finds both scoped and unscoped plugins with versions, ignoring other packages", () => {
		const dir = tmp("list-mixed");
		writePkg(dir, "punktfunk-plugin-custom", "1.2.3");
		writePkg(dir, path.join("@punktfunk", "plugin-playnite"), "0.2.0");
		writePkg(dir, "effect", "4.0.0"); // an ordinary dep — must not be listed
		writePkg(dir, path.join("@punktfunk", "host"), "0.1.1"); // scoped non-plugin — ignored

		const found = listInstalled(dir);
		expect(found).toEqual([
			{ pkg: "@punktfunk/plugin-playnite", version: "0.2.0" },
			{ pkg: "punktfunk-plugin-custom", version: "1.2.3" },
		]);
	});

	test("tolerates a plugin with an unreadable package.json", () => {
		const dir = tmp("list-broken");
		fs.mkdirSync(path.join(dir, "node_modules", "punktfunk-plugin-broken"), {
			recursive: true,
		});
		expect(listInstalled(dir)).toEqual([
			{ pkg: "punktfunk-plugin-broken", version: undefined },
		]);
	});

	test("finds plugins in ANY scope, not just @punktfunk", () => {
		// Plugin-store catalog entries must be scoped so the scope can map to that entry's
		// registry, so a third-party plugin necessarily arrives as `@their-scope/plugin-*`.
		// Discovery limited to @punktfunk would let it install and then never run.
		const dir = tmp("list-foreign-scope");
		writePkg(dir, path.join("@retro-hub", "plugin-x"), "1.0.0");
		writePkg(dir, path.join("@retro-hub", "helper"), "1.0.0"); // scoped non-plugin — ignored
		expect(listInstalled(dir)).toEqual([{ pkg: "@retro-hub/plugin-x", version: "1.0.0" }]);
	});
});

describe("ensureBunfig with extra scopes", () => {
	const read = (dir: string) => fs.readFileSync(path.join(dir, "bunfig.toml"), "utf8");

	test("maps a third-party scope alongside @punktfunk", () => {
		const dir = tmp("bunfig-extra");
		ensureBunfig(dir, { "@retro-hub": "https://retro.example/npm/" });
		const out = read(dir);
		expect(out).toContain(`"@punktfunk" = "${REGISTRY}"`);
		expect(out).toContain('"@retro-hub" = "https://retro.example/npm/"');
	});

	test("is idempotent and rewrites a scope whose registry changed", () => {
		const dir = tmp("bunfig-rewrite");
		ensureBunfig(dir, { "@retro-hub": "https://old.example/npm/" });
		ensureBunfig(dir, { "@retro-hub": "https://old.example/npm/" });
		expect(read(dir).match(/@retro-hub/g)?.length).toBe(1);

		ensureBunfig(dir, { "@retro-hub": "https://new.example/npm/" });
		const out = read(dir);
		expect(out).toContain('"@retro-hub" = "https://new.example/npm/"');
		expect(out).not.toContain("old.example");
		// the first-party mapping survives an unrelated scope edit
		expect(out).toContain(`"@punktfunk" = "${REGISTRY}"`);
	});

	test("preserves unrelated scopes already in the file", () => {
		const dir = tmp("bunfig-preserve");
		fs.writeFileSync(
			path.join(dir, "bunfig.toml"),
			'[install.scopes]\n"@acme" = "https://acme.example/"\n',
		);
		ensureBunfig(dir, { "@retro-hub": "https://retro.example/npm/" });
		const out = read(dir);
		expect(out).toContain('"@acme" = "https://acme.example/"');
		expect(out).toContain('"@retro-hub" = "https://retro.example/npm/"');
		expect(out).toContain(`"@punktfunk" = "${REGISTRY}"`);
	});
});

describe("ensurePluginsDir", () => {
	test("creates the dir (and parents) and returns it", () => {
		const dir = path.join(tmp("ensure-dir"), "nested", "plugins");
		expect(ensurePluginsDir(dir)).toBe(dir);
		expect(fs.statSync(dir).isDirectory()).toBe(true);
		ensurePluginsDir(dir); // idempotent
	});

	// Field bug 2026-07-31: `bun add` installs into the nearest ancestor package.json, not into
	// its working dir. With none here, a stray ~/package.json captured every plugin install — bun
	// exited 0, the packages landed in the home dir, and the plugins dir stayed empty.
	test("seeds a package.json so bun cannot install into an ancestor", () => {
		const dir = path.join(tmp("ensure-root"), "plugins");
		ensurePluginsDir(dir);
		const seeded = JSON.parse(
			fs.readFileSync(path.join(dir, "package.json"), "utf8"),
		) as { name: string; private: boolean };
		expect(seeded.name).toBe("punktfunk-plugins");
		expect(seeded.private).toBe(true);
	});

	test("never overwrites an existing package.json", () => {
		const dir = tmp("ensure-keep");
		const manifest = '{"dependencies":{"@punktfunk/plugin-playnite":"0.3.0"}}';
		fs.writeFileSync(path.join(dir, "package.json"), manifest);
		ensurePluginsDir(dir);
		expect(fs.readFileSync(path.join(dir, "package.json"), "utf8")).toBe(manifest);
	});

	// The one tree we must not seed: packages present, no package.json. Discovery falls back to
	// the naming convention there, and an empty `dependencies` would report every installed plugin
	// as gone (the host's installed-package scan narrows to that list).
	test("leaves a tree that already has packages alone", () => {
		const dir = tmp("ensure-existing");
		writePkg(dir, "punktfunk-plugin-legacy", "0.1.0");
		ensurePluginsDir(dir);
		expect(fs.existsSync(path.join(dir, "package.json"))).toBe(false);
		expect(listInstalled(dir).map((p) => p.pkg)).toEqual([
			"punktfunk-plugin-legacy",
		]);
	});
});

describe("stripGroupWrite", () => {
	test.skipIf(process.platform === "win32")(
		"clears group/world write on the tree a umask-002 install left behind",
		() => {
			const dir = tmp("strip-go-w");
			writePkg(dir, "@punktfunk/plugin-x", "1.0.0");
			const pkgDir = path.join(dir, "node_modules", "@punktfunk", "plugin-x");
			const entry = path.join(pkgDir, "index.js");
			fs.writeFileSync(entry, "export default {};\n");
			fs.chmodSync(entry, 0o664);
			fs.chmodSync(pkgDir, 0o775);
			// A symlink is skipped, and its target outside the tree is left alone.
			const outside = path.join(dir, "outside.js");
			fs.writeFileSync(outside, "");
			fs.chmodSync(outside, 0o666);
			fs.symlinkSync(outside, path.join(pkgDir, "link.js"));

			stripGroupWrite(dir);

			expect(fs.statSync(entry).mode & 0o022).toBe(0);
			expect(fs.statSync(pkgDir).mode & 0o022).toBe(0);
			expect(fs.statSync(entry).mode & 0o644).toBe(0o644);
			expect(fs.statSync(outside).mode & 0o022).toBe(0o022);
			// No node_modules at all is not an error.
			stripGroupWrite(tmp("strip-empty"));
		},
	);
});
