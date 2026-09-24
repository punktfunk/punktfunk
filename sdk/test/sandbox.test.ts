// What a plugin can reach is decided entirely by this argv, so it is worth pinning: an empty home,
// its own state and token, the paths it declared — and nothing that was not asked for.
import { describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import {
	bwrapArgv,
	expandHome,
	grantedRoots,
	netlinkFilter,
	type PluginManifest,
	readManifest,
	refusedRoot,
	sandboxEnv,
	sandboxProbe,
} from "../src/sandbox.js";

const paths = {
	stateDir: "/home/u/.config/punktfunk/plugin-state/demo",
	tokenFile: "/home/u/.config/punktfunk/plugin-state/demo/.plugin-token",
	socket: "/run/user/1000/punktfunk/plugin-demo.sock",
	pluginsDir: "/home/u/.config/punktfunk/plugins",
	bun: "/usr/lib/punktfunk-bun/bun",
	runner: "/usr/share/punktfunk-scripting/runner-cli.js",
	home: "/home/u",
};

const manifest = (over: Partial<PluginManifest> = {}): PluginManifest => ({
	schema: 1,
	id: "demo",
	reads: ["~/.local/share/Steam"],
	...over,
});

/** `--flag a b` → the pairs for that flag. */
const binds = (argv: string[], flag: string): Array<[string, string]> => {
	const out: Array<[string, string]> = [];
	for (let i = 0; i < argv.length; i++) {
		if (argv[i] === flag) out.push([argv[i + 1] as string, argv[i + 2] as string]);
	}
	return out;
};

describe("refusedRoot", () => {
	test("keeps the host's processes, sockets, keys and credentials out of every sandbox", () => {
		for (const p of [
			"/",
			"/home",
			"/home/u",
			"/proc",
			"/proc/1/root",
			"/sys/kernel",
			"/dev/shm",
			"/run",
			"/run/media",
			"/run/user/1000",
			"/home/u/.ssh",
			"/home/u/.gnupg/private-keys-v1.d",
			"/home/u/.config/punktfunk",
			"/home/u/.config/punktfunk/plugin-run",
			"/home/u/.config/punktfunk-extra",
			"/home/u/Games/../.ssh",
		]) {
			expect([p, refusedRoot(p, "/home/u")]).toEqual([p, true]);
		}
	});

	test("lets launcher, media and temp paths through", () => {
		for (const p of [
			"/home/u/.local/share/Steam",
			"/home/u/Emu",
			"/run/media/u/SD",
			"/mnt/games1",
			"/tmp/vhclient_response",
			"/usr/share/applications",
		]) {
			expect([p, refusedRoot(p, "/home/u")]).toEqual([p, false]);
		}
	});

	test("sees the home behind a link, as on Fedora Atomic", () => {
		const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "pf-linkhome-")));
		const real = path.join(root, "var/home/u");
		fs.mkdirSync(path.join(real, ".ssh"), { recursive: true });
		fs.mkdirSync(path.join(real, "Games"), { recursive: true });
		fs.symlinkSync(path.join(root, "var/home"), path.join(root, "home"));
		const home = path.join(root, "home/u");
		try {
			expect(refusedRoot(real, home)).toBe(true);
			expect(refusedRoot(path.join(real, ".ssh"), home)).toBe(true);
			expect(refusedRoot(path.join(real, "Games"), home)).toBe(false);
		} finally {
			fs.rmSync(root, { recursive: true, force: true });
		}
	});

	test("a manifest or grant naming a refused root is never bound", () => {
		const argv = bwrapArgv(
			manifest({ reads: ["/proc", "~/.config/punktfunk/plugin-run", "~/Games"] }),
			paths,
			[{ path: "/run/user/1000", write: false }],
		);
		const bound = binds(argv, "--ro-bind-try").map(([src]) => src);
		expect(bound).toContain("/home/u/Games");
		expect(bound).not.toContain("/proc");
		expect(bound).not.toContain("/home/u/.config/punktfunk/plugin-run");
		expect(bound).not.toContain("/run/user/1000");
	});
});

describe("readManifest", () => {
	test("refuses an id that is not one lowercase path component", () => {
		const dir = fs.mkdtempSync(path.join(os.tmpdir(), "pf-manifest-"));
		const write = (id: string) =>
			fs.writeFileSync(
				path.join(dir, "package.json"),
				JSON.stringify({ punktfunk: { schema: 1, id } }),
			);
		try {
			write("rom-manager");
			expect(readManifest(dir)?.id).toBe("rom-manager");
			for (const bad of ["..", "../plugins", "Steam", "a/b", ""]) {
				write(bad);
				expect([bad, readManifest(dir)]).toEqual([bad, undefined]);
			}
		} finally {
			fs.rmSync(dir, { recursive: true, force: true });
		}
	});
});

describe("bwrapArgv", () => {
	test("a Nix-built bun gets the store it links against", () => {
		const nix = { ...paths, bun: "/nix/store/abc-bun-1.3/bin/bun" };
		expect(binds(bwrapArgv(manifest(), nix), "--ro-bind")).toContainEqual([
			"/nix/store",
			"/nix/store",
		]);
		expect(bwrapArgv(manifest(), paths).join(" ")).not.toContain("/nix/store");
	});

	test("takes away the namespaces the boundary depends on", () => {
		const argv = bwrapArgv(manifest(), paths);
		// `--unshare-all` plus a fresh /proc is what stops the same uid reaching the host process
		// through /proc/<pid>/root, /proc/<pid>/environ or kill(2).
		expect(argv).toContain("--unshare-all");
		expect(argv).toContain("--disable-userns");
		// bwrap refuses `--disable-userns` unless a user namespace is DEMANDED: `--unshare-all`
		// only tries for one. Without this pair every plugin exits 1 before it starts.
		expect(argv).toContain("--unshare-user");
		expect(argv).toContain("--die-with-parent");
		expect(argv).toContain("--clearenv");
		// …which empties the child's environment, so every value must be re-stated in the argv.
		// The spawn env does not survive it: without these the plugin has no HOME and no socket.
		const joined = argv.join(" ");
		expect(joined).toContain("--setenv HOME");
		expect(joined).toContain("--setenv PUNKTFUNK_MGMT_UNIX /run/punktfunk/host.sock");
		expect(joined).toContain("--setenv PUNKTFUNK_CONFIG_DIR /run/punktfunk");
		expect(binds(argv, "--proc")).toBeDefined();
		expect(argv.join(" ")).toContain("--proc /proc");
		// The unit allows netlink and writable tunables for bwrap's setup; the plugin gets neither.
		expect(joined).toContain("--add-seccomp-fd 3");
		expect(binds(argv, "--ro-bind")).toContainEqual(["/proc/sys", "/proc/sys"]);
	});

	test("binds the plugin's own things, and the home only through what it declared", () => {
		const argv = bwrapArgv(manifest(), paths);
		// The kit's `pluginStateDir("demo")` inside the sandbox, not one level above it.
		expect(binds(argv, "--bind")).toContainEqual([
			paths.stateDir,
			"/run/punktfunk/plugin-state/demo",
		]);
		expect(binds(argv, "--ro-bind")).toContainEqual([
			paths.tokenFile,
			"/run/punktfunk/plugin-token",
		]);
		expect(binds(argv, "--bind")).toContainEqual([paths.socket, "/run/punktfunk/host.sock"]);
		expect(binds(argv, "--ro-bind-try")).toContainEqual([
			"/home/u/.local/share/Steam",
			"/home/u/.local/share/Steam",
		]);
		// The home itself is never bound, so neither is ~/.ssh or ~/.config/punktfunk.
		const all = [...binds(argv, "--bind"), ...binds(argv, "--ro-bind"), ...binds(argv, "--ro-bind-try")];
		expect(all.map(([src]) => src)).not.toContain("/home/u");
		expect(all.map(([src]) => src)).not.toContain("/home/u/.config/punktfunk");
	});

	test("no network unless the manifest asked for one", () => {
		expect(bwrapArgv(manifest(), paths)).not.toContain("--share-net");
		expect(bwrapArgv(manifest({ network: true }), paths)).toContain("--share-net");
	});

	test("without network the UI gets the runner's socket dir and port", () => {
		const ui = { dir: "/run/user/1000/punktfunk/ui-demo-ab12", port: 41234 };
		const argv = bwrapArgv(manifest(), { ...paths, ui }).join(" ");
		expect(argv).toContain(`--bind ${ui.dir} /run/punktfunk/ui`);
		expect(argv).toContain("--setenv PUNKTFUNK_UI_PORT 41234");
		// A plugin on the host's network serves its UI on the host's loopback itself.
		const shared = bwrapArgv(manifest({ network: true }), { ...paths, ui }).join(" ");
		expect(shared).not.toContain("PUNKTFUNK_UI_PORT");
	});

	test("grants bind read-only by default and writable only when they say so", () => {
		const argv = bwrapArgv(manifest({ writes: ["/tmp/vhclient"] }), paths, [
			{ path: "/mnt/legacy", write: false },
			{ path: "/mnt/write", write: true },
			{ path: "not/absolute", write: false },
		]);
		expect(binds(argv, "--bind-try")).toContainEqual(["/tmp/vhclient", "/tmp/vhclient"]);
		expect(binds(argv, "--ro-bind-try")).toContainEqual(["/mnt/legacy", "/mnt/legacy"]);
		expect(binds(argv, "--bind-try")).toContainEqual(["/mnt/write", "/mnt/write"]);
		expect(argv.join(" ")).not.toContain("not/absolute");
	});
});

describe("grantedRoots", () => {
	test("parses v1 path arrays and v2 grant records, and nothing malformed", () => {
		const dir = fs.mkdtempSync(path.join(os.tmpdir(), "grants-"));
		try {
			fs.mkdirSync(path.join(dir, "plugin-run"));
			const write = (v: unknown) =>
				fs.writeFileSync(path.join(dir, "plugin-run", "plugin-grants.json"), JSON.stringify(v));
			write({ demo: ["/mnt/old"] });
			expect(grantedRoots(dir, "demo")).toEqual([{ path: "/mnt/old", write: false }]);
			write({
				demo: {
					grants: [
						{ path: "/mnt/read", write: false },
						{ path: "/mnt/write", write: true },
					],
					denied: [],
				},
			});
			expect(grantedRoots(dir, "demo")).toEqual([
				{ path: "/mnt/read", write: false },
				{ path: "/mnt/write", write: true },
			]);
			fs.writeFileSync(path.join(dir, "plugin-run", "plugin-grants.json"), "{not json");
			expect(grantedRoots(dir, "demo")).toEqual([]);
			write({ demo: { grants: [{ path: "/mnt/x", write: "yes" }] } });
			expect(grantedRoots(dir, "demo")).toEqual([]);
		} finally {
			fs.rmSync(dir, { recursive: true, force: true });
		}
	});
});

describe("sandboxEnv", () => {
	test("carries no inherited value, and points the SDK at the socket", () => {
		const env = sandboxEnv("/home/u");
		// The real home, so a `~/...` read the manifest declared is where os.homedir() looks.
		expect(env.HOME).toBe("/home/u");
		expect(env.PUNKTFUNK_CONFIG_DIR).toBe("/run/punktfunk");
		expect(env.PUNKTFUNK_MGMT_UNIX).toBe("/run/punktfunk/host.sock");
		expect(env.PUNKTFUNK_MGMT_TOKEN).toBeUndefined();
	});
});

describe("expandHome", () => {
	test("resolves ~ and leaves an absolute path alone", () => {
		expect(expandHome("~/.config/x", "/home/u")).toBe("/home/u/.config/x");
		expect(expandHome("/opt/x", "/home/u")).toBe("/opt/x");
	});
});

describe("sandboxProbe", () => {
	test("says which of the two ways it is unavailable", () => {
		expect(sandboxProbe(() => ({ status: 0 }), "darwin").ok).toBe(false);
		expect(sandboxProbe(() => ({ status: 0 }), "linux")).toEqual({ ok: true });
		// The probe must ask for what bwrapArgv asks for, or it calls a box capable that then
		// refuses every plugin.
		let probed: string[] = [];
		sandboxProbe((_cmd, args) => {
			probed = args;
			return { status: 0 };
		}, "linux");
		expect(probed).toContain("--unshare-user");
		expect(probed).toContain("--disable-userns");
		expect(probed).toContain("/nix");
		expect(probed).toContain("/run/current-system");
		expect(probed.at(-1)).toMatch(/true$/);
		// The store binds are probe plumbing; a plugin's sandbox keeps them out.
		expect(bwrapArgv({ schema: 1, id: "x" }, paths)).not.toContain("/nix");
		const missing = sandboxProbe(() => ({ status: null }), "linux");
		expect(missing.ok).toBe(false);
		expect(!missing.ok && missing.reason).toContain("bubblewrap");
		const killed = sandboxProbe(
			() => ({ status: null, signal: "SIGKILL" }),
			"linux",
		);
		expect(!killed.ok && killed.reason).toContain("SIGKILL");
		const denied = sandboxProbe(() => ({ status: 1 }), "linux");
		expect(!denied.ok && denied.reason).toContain("user namespaces");
		const execFail = sandboxProbe(
			() => ({
				status: 1,
				stderr: "bwrap: execvp /bin/true: No such file or directory\n",
			}),
			"linux",
		);
		expect(!execFail.ok && execFail.reason).toContain("execvp");
	});
});

describe("netlinkFilter", () => {
	/** Run the program over one `seccomp_data`: arch, syscall number, first argument. */
	const verdict = (prog: Buffer, arch: number, nr: number, arg0: number): number => {
		const data = Buffer.alloc(64);
		data.writeUInt32LE(nr, 0);
		data.writeUInt32LE(arch, 4);
		data.writeUInt32LE(arg0, 16);
		let acc = 0;
		for (let pc = 0; pc * 8 < prog.length; pc++) {
			const [code, jt, jf, k] = [
				prog.readUInt16LE(pc * 8),
				prog.readUInt8(pc * 8 + 2),
				prog.readUInt8(pc * 8 + 3),
				prog.readUInt32LE(pc * 8 + 4),
			];
			if (code === 0x06) return k;
			if (code === 0x20) acc = data.readUInt32LE(k);
			else if (code === 0x15) pc += acc === k ? jt : jf;
			else if (code === 0x35) pc += acc >= k ? jt : jf;
			else throw new Error(`opcode ${code}`);
		}
		throw new Error("fell off the end");
	};
	const ALLOW = 0x7fff0000;
	const DENY = 0x00050000 | 97;

	test("refuses a netlink socket and nothing else a plugin needs", () => {
		for (const [arch, audit, socket] of [
			["x64", 0xc000003e, 41],
			["arm64", 0xc00000b7, 198],
		] as const) {
			const prog = netlinkFilter(arch) as Buffer;
			expect(verdict(prog, audit, socket, 16)).toBe(DENY); // AF_NETLINK
			expect(verdict(prog, audit, socket, 2)).toBe(ALLOW); // AF_INET
			expect(verdict(prog, audit, socket, 1)).toBe(ALLOW); // AF_UNIX
			expect(verdict(prog, audit, 0, 16)).toBe(ALLOW); // another syscall, same argument
			expect(verdict(prog, audit, 0x40000000 + socket, 16)).toBe(DENY); // x32
			expect(verdict(prog, 0x40000003, 102, 16)).toBe(DENY); // i386 socketcall
		}
		expect(netlinkFilter("riscv64")).toBeUndefined();
	});
});
