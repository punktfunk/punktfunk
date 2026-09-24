// One sandbox per plugin (design/host-trust-boundaries.md §3.1).
//
// The runner is a `systemd --user` unit, so it shares the operator's uid — and a mount namespace
// alone is no boundary against the same uid: `/proc/<host pid>/root` reaches the files it hides,
// and `/proc/<pid>/environ` and `kill` pass the same check. What makes this one hold is the pid
// namespace: with `--unshare-pid` and a fresh `/proc`, the host's process does not exist inside,
// so there is nothing to reach through.
//
// Each plugin gets: an empty home, its own state dir, the paths its manifest declares, its own
// token, and a unix socket to the host. No network unless it declared one; then its UI reaches the
// console through a second socket (`ui-forward.ts`). Pin: `sandbox.test.ts`.
import { spawnSync } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";
import { UI_DIR } from "./ui-forward.js";

/** A plugin's `punktfunk` block — the half of it a sandbox is built from. */
export interface PluginManifest {
	schema?: number;
	id?: string;
	reads?: string[];
	writes?: string[];
	network?: boolean;
}

/** An id names the plugin's state dir and socket: one lowercase path component. */
const PLUGIN_ID = /^[a-z0-9][a-z0-9-]{0,63}$/;

/** Read `package.json`'s `punktfunk` block, or `undefined` when there is none or its id is unusable. */
export const readManifest = (packageDir: string): PluginManifest | undefined => {
	try {
		const pkg = JSON.parse(
			fs.readFileSync(path.join(packageDir, "package.json"), "utf8"),
		) as { punktfunk?: PluginManifest };
		const m = pkg.punktfunk;
		return m && m.schema === 1 && typeof m.id === "string" && PLUGIN_ID.test(m.id)
			? m
			: undefined;
	} catch {
		return undefined;
	}
};

/** `~/x` → `<home>/x`. A path without the prefix is already absolute or is skipped. */
export const expandHome = (p: string, home: string): string =>
	p.startsWith("~/") ? path.join(home, p.slice(2)) : p;

/** One extra root the operator granted a plugin, read-only unless `write` is set. */
export interface GrantedRoot {
	path: string;
	write: boolean;
}

export interface SandboxPaths {
	/** Where this plugin's own files live, bound read-write. */
	stateDir: string;
	/** The file holding this plugin's token, bound read-only as its `plugin-token`. */
	tokenFile: string;
	/** The unix socket the supervisor proxies to the host on. */
	socket: string;
	/** The plugin install root (`<config>/plugins`), bound read-only: its code. */
	pluginsDir: string;
	/** The runtime and the runner bundle the child re-execs. */
	bun: string;
	runner: string;
	home: string;
	/** A plugin without network: the runner's dir its UI socket goes in, and the forwarded port. */
	ui?: { dir: string; port: number };
}

/**
 * The `bwrap` argv for one plugin: everything before the program it runs.
 *
 * Read-only for the system, the plugin's own code, manifest `reads`, and granted roots; read-write
 * for exactly its state dir, manifest `writes`, grants marked `write: true`, and `/tmp` (VirtualHere's
 * client IPC is a FIFO pair there, which is why the unit keeps the real `/tmp`).
 */
/**
 * The namespaces and the minimal root every sandbox gets, shared with {@link sandboxProbe} so a
 * box is never called capable on flags the real sandbox does not use — and so a flag added here
 * is a flag the probe actually exercises.
 *
 * The `/lib` + `/lib64` symlinks are not cosmetic: with only `/usr` bound, the dynamic loader is
 * absent and every exec fails as ENOENT, which reads exactly like a refused namespace.
 */
const BASE_ARGV: readonly string[] = [
	"--unshare-all",
	// `--unshare-all` only asks for a user namespace (`--unshare-user-try`), and
	// `--disable-userns` refuses to run without a real one. Demand it explicitly.
	"--unshare-user",
	// A plugin cannot re-enter this and build itself a wider one.
	"--disable-userns",
	"--die-with-parent",
	"--new-session",
	"--clearenv",
	// `--unshare-pid` is what makes the rest hold: a fresh /proc has no host process in it.
	"--proc",
	"/proc",
	// The unit cannot keep tunables read-only (ProtectKernelTunables refuses the mount above).
	"--ro-bind",
	"/proc/sys",
	"/proc/sys",
	"--dev",
	"/dev",
	"--tmpfs",
	"/tmp",
	"--ro-bind",
	"/usr",
	"/usr",
	"--ro-bind-try",
	"/etc/ssl",
	"/etc/ssl",
	"--ro-bind-try",
	"/etc/resolv.conf",
	"/etc/resolv.conf",
	"--symlink",
	"usr/lib",
	"/lib",
	"--symlink",
	"usr/lib64",
	"/lib64",
	"--symlink",
	"usr/bin",
	"/bin",
	"--symlink",
	"usr/sbin",
	"/sbin",
];

export const bwrapArgv = (
	manifest: PluginManifest,
	paths: SandboxPaths,
	grants: readonly GrantedRoot[] = [],
): string[] => {
	// The spawner hands `netlinkFilter()` over on fd 3.
	const argv = [...BASE_ARGV, "--add-seccomp-fd", "3"];
	// `--clearenv` empties the environment the child sees, so the spawn env never reaches it:
	// every value has to be re-stated here. Without this a plugin has no HOME, cannot resolve
	// its state dir, and cannot find the socket it reaches the host on.
	for (const [k, v] of Object.entries(sandboxEnv(paths.home))) {
		argv.push("--setenv", k, v);
	}
	if (manifest.network) argv.push("--share-net");
	// Its own code, its own state, its own token, and the way to the host.
	argv.push("--ro-bind", paths.pluginsDir, paths.pluginsDir);
	argv.push("--ro-bind", paths.bun, paths.bun);
	argv.push("--ro-bind", paths.runner, paths.runner);
	// A Nix-built bun loads its libc from the store, and NixOS tools live under the system
	// profile. Both are world-readable already.
	if (paths.bun.startsWith("/nix/store/")) {
		argv.push("--ro-bind", "/nix/store", "/nix/store");
		argv.push("--ro-bind-try", "/run/current-system", "/run/current-system");
	}
	// Where `pluginStateDir(<id>)` resolves inside: `PUNKTFUNK_CONFIG_DIR/plugin-state/<id>`.
	argv.push(
		"--bind",
		paths.stateDir,
		path.join("/run/punktfunk/plugin-state", path.basename(paths.stateDir)),
	);
	argv.push("--ro-bind", paths.tokenFile, "/run/punktfunk/plugin-token");
	argv.push("--bind", paths.socket, "/run/punktfunk/host.sock");
	// Its own loopback is unreachable, so its UI listens where the runner can forward to.
	if (paths.ui && !manifest.network) {
		argv.push("--bind", paths.ui.dir, UI_DIR);
		argv.push("--setenv", "PUNKTFUNK_UI_PORT", String(paths.ui.port));
	}
	// What it said it needs. `-try` so an uninstalled launcher's dir is simply absent rather
	// than a sandbox that refuses to start.
	for (const p of manifest.reads ?? []) {
		const abs = expandHome(p, paths.home);
		if (bindable(abs, paths.home)) argv.push("--ro-bind-try", abs, abs);
	}
	for (const p of manifest.writes ?? []) {
		const abs = expandHome(p, paths.home);
		if (bindable(abs, paths.home)) argv.push("--bind-try", abs, abs);
	}
	// Operator grants are read-only unless one opts into write.
	for (const grant of grants) {
		const abs = expandHome(grant.path, paths.home);
		if (bindable(abs, paths.home))
			argv.push(grant.write ? "--bind-try" : "--ro-bind-try", abs, abs);
	}
	return argv;
};

/**
 * A root no manifest or grant may bind: the host's processes, devices, the session bus and
 * runtime sockets, the home or anything above it, keys, and punktfunk's own config, which holds
 * every plugin's token. Checked on the path and on what it resolves to in the runner's view.
 */
export const refusedRoot = (abs: string, home: string): boolean => {
	const refused = (p: string, h: string) => {
		const under = (base: string) => p === base || p.startsWith(`${base}/`);
		return (
			p === "/" ||
			`${h}/`.startsWith(`${p}/`) ||
			["/proc", "/sys", "/dev"].some(under) ||
			(under("/run") && !p.startsWith("/run/media/")) ||
			[".ssh", ".gnupg"].some((d) => under(path.join(h, d))) ||
			p.startsWith(path.join(h, ".config", "punktfunk"))
		);
	};
	const real = (p: string) => {
		try {
			return fs.realpathSync(p);
		} catch {
			return p;
		}
	};
	// Both spellings of both sides: on Fedora Atomic `/home` is a link to `/var/home`.
	const p = path.resolve(abs);
	const paths = [p, real(p)];
	const homes = [home, real(home)];
	return paths.some((x) => homes.some((h) => refused(x, h)));
};

const bindable = (abs: string, home: string): boolean =>
	path.isAbsolute(abs) && !refusedRoot(abs, home);

/**
 * A seccomp program refusing netlink sockets, as the bytes bwrap reads from `--add-seccomp-fd`.
 *
 * The runner's unit allows AF_NETLINK only because bwrap needs it to bring up the sandbox's
 * loopback; bwrap loads this after that setup, so the plugin never gets it. Classic BPF over
 * `seccomp_data`: a foreign architecture or any x32 call is refused outright, then `socket` with
 * a netlink domain. `undefined` on an architecture with no table entry.
 */
export const netlinkFilter = (arch: string = process.arch): Buffer | undefined => {
	// AUDIT_ARCH_* and __NR_socket.
	const native = ({ x64: [0xc000003e, 41], arm64: [0xc00000b7, 198] } as const)[
		arch as "x64" | "arm64"
	];
	if (!native) return undefined;
	const [LOAD, JEQ, JGE, RET] = [0x20, 0x15, 0x35, 0x06];
	const DENY = 0x00050000 | 97; // SECCOMP_RET_ERRNO | EAFNOSUPPORT
	const ALLOW = 0x7fff0000;
	// [code, jump-if-true, jump-if-false, k]; a jump skips that many instructions.
	const program = [
		[LOAD, 0, 0, 4], // arch
		[JEQ, 0, 5, native[0]],
		[LOAD, 0, 0, 0], // syscall number
		[JGE, 3, 0, 0x40000000], // x32
		[JEQ, 0, 3, native[1]],
		[LOAD, 0, 0, 16], // low word of the first argument: the domain
		[JEQ, 0, 1, 16], // AF_NETLINK
		[RET, 0, 0, DENY],
		[RET, 0, 0, ALLOW],
	];
	const out = Buffer.alloc(program.length * 8);
	program.forEach(([code, jt, jf, k], i) => {
		out.writeUInt16LE(code, i * 8);
		out.writeUInt8(jt, i * 8 + 2);
		out.writeUInt8(jf, i * 8 + 3);
		out.writeUInt32LE(k, i * 8 + 4);
	});
	return out;
};

/** The environment inside: no inherited values, and nothing that is not needed there. */
export const sandboxEnv = (
	home: string,
	extra: Record<string, string> = {},
): Record<string, string> => ({
	// The REAL home, which is where a `~/...` read is bound. A plugin finds its declared paths
	// with os.homedir(); pointing this at the state dir sends every scanner somewhere empty.
	// Writable state is PUNKTFUNK_CONFIG_DIR's job, not this one's.
	HOME: home,
	// sbin: Debian ships VirtualHere's client there. The system profile is NixOS's /usr/bin.
	PATH: "/usr/bin:/bin:/usr/local/bin:/usr/sbin:/sbin:/run/current-system/sw/bin",
	PUNKTFUNK_CONFIG_DIR: "/run/punktfunk",
	// Reached through the supervisor's socket, so plain HTTP with no credential of its own.
	PUNKTFUNK_MGMT_URL: "http://punktfunk.host",
	PUNKTFUNK_MGMT_UNIX: "/run/punktfunk/host.sock",
	...extra,
});

/**
 * The operator's extra roots for `id` from `plugin-run/plugin-grants.json`. Accepts a legacy array
 * (read-only grants) and a `{ grants: [{ path, write }] }` record. Anything else — malformed JSON,
 * a missing entry, a shape that does not parse strictly — grants nothing.
 */
export const grantedRoots = (configDir: string, id: string): GrantedRoot[] => {
	try {
		const map = JSON.parse(
			fs.readFileSync(path.join(configDir, "plugin-run", "plugin-grants.json"), "utf8"),
		) as Record<string, unknown>;
		const entry = map[id];
		if (Array.isArray(entry)) {
			if (!entry.every((p) => typeof p === "string")) return [];
			return entry.map((p) => ({ path: p, write: false }));
		}
		if (typeof entry !== "object" || entry === null) return [];
		const grants = (entry as { grants?: unknown }).grants;
		if (!Array.isArray(grants)) return [];
		if (
			!grants.every(
				(g) =>
					typeof g === "object" &&
					g !== null &&
					!Array.isArray(g) &&
					typeof (g as GrantedRoot).path === "string" &&
					typeof (g as GrantedRoot).write === "boolean",
			)
		)
			return [];
		return grants as GrantedRoot[];
	} catch {
		return [];
	}
};

// NixOS has no FHS /bin/true and no ld-linux under /usr. A store `true` needs /nix
// for its loader, /run/current-system for the profile symlink. Probe scope only —
// plugins do not get the whole store read-only for a diagnostic.
const PROBE_BINDS: readonly string[] = [
	"--ro-bind-try",
	"/nix",
	"/nix",
	"--ro-bind-try",
	"/run/current-system",
	"/run/current-system",
];

/** Host `true` as an absolute path. `/bin/true` is an FHS path NixOS does not have.
 *  Do not realpath: Nix `true` is a symlink onto the coreutils multicall binary. */
const whichTrue = (): string => {
	for (const dir of (process.env.PATH ?? "").split(path.delimiter)) {
		if (!dir) continue;
		const candidate = path.join(dir, "true");
		try {
			// A file with an exec bit — a directory or unexecutable `true` on PATH
			// would make a healthy sandbox read as an exec failure.
			fs.accessSync(candidate, fs.constants.X_OK);
			if (fs.statSync(candidate).isFile()) return candidate;
		} catch {
			/* absent, dangling, or not executable */
		}
	}
	return "/bin/true";
};

/** Can this box sandbox at all? The reason, when it cannot, is what the operator needs. */
export const sandboxProbe = (
	run: (
		cmd: string,
		args: string[],
	) => { status: number | null; signal?: string | null; stderr?: string } = (
		cmd,
		args,
	) => {
		const r = spawnSync(cmd, args, { encoding: "utf8" });
		return {
			status: r.error ? null : r.status,
			signal: r.signal,
			stderr: r.stderr ?? "",
		};
	},
	platform: string = process.platform,
): { ok: true } | { ok: false; reason: string } => {
	if (platform !== "linux") {
		return { ok: false, reason: "sandboxing is Linux-only here" };
	}
	// The namespace flags `bwrapArgv` actually uses. Probing a weaker set reports a box as
	// sandbox-capable that then refuses every plugin.
	// Exactly what a real sandbox asks for, plus a trivial exec. Probing a weaker set reports a
	// box as capable that then refuses every plugin.
	const trueBin = whichTrue();
	// A `true` under a PATH dir the sandbox does not bind (a ~/bin on FHS) still execs:
	// its parent comes along.
	const dir = path.dirname(trueBin);
	const probe = run("bwrap", [
		...BASE_ARGV,
		...PROBE_BINDS,
		...(dir === "/" ? [] : ["--ro-bind-try", dir, dir]),
		trueBin,
	]);
	if (probe.status === 0) return { ok: true };
	if (probe.status === null) {
		return {
			ok: false,
			reason: probe.signal
				? `bwrap was killed by ${probe.signal} before it could report`
				: "bubblewrap (bwrap) is not installed — install it, or set PUNKTFUNK_PLUGIN_SANDBOX=off",
		};
	}
	const line = (probe.stderr ?? "").split("\n")[0]?.trim();
	return {
		ok: false,
		reason:
			line ||
			"bwrap could not create a namespace — this kernel restricts unprivileged user namespaces",
	};
};
