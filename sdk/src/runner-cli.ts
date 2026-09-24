#!/usr/bin/env bun
// `punktfunk-scripting` — the plugin/script runner AND the `punktfunk-host plugins …` package ops.
//
// With NO subcommand it RUNS the runner: discover the operator's scripts + punktfunk-plugin-*
// packages and supervise them (see ./runner.ts). SIGINT/SIGTERM interrupt the whole tree
// structurally, so every plugin's finalizers run before exit (the systemd-stop story). This bare
// form is what the systemd unit / Windows scheduled task launch — do not change its behavior.
//
// With a subcommand it manages plugin packages (the host CLI forwards `punktfunk-host plugins …`
// here):
//   add <name…>      install first-party plugins (playnite, rom-manager); anything resolving on
//                    the PUBLIC npm registry (punktfunk-plugin-*, foreign scopes) additionally
//                    needs --allow-public-registry
//   remove <name…>   uninstall
//   list             list installed plugin packages
//
//   bun src/runner-cli.ts [--scripts DIR] [--plugins DIR] [--list]   (run the runner)
//   bun src/runner-cli.ts add playnite [--plugins DIR]               (package ops)
//
// Package-op flags: --exact pins the resolved version instead of a caret range, and
// --registry @scope=https://… maps a scope to its registry in bunfig.toml. Both exist for the
// plugin store (crates/punktfunk-host/src/store), which installs one reviewed version of a
// package that may live on somebody else's registry — but they are ordinary CLI flags too.
import { Effect, Fiber } from "effect";
import { publishedMgmtUrl } from "./config.js";
import { installLogShipper } from "./log-ship.js";
import {
	addPlugins,
	listInstalled,
	reconcileSharedSdk,
	removePlugins,
} from "./plugins.js";
import { discoverUnits, runner, runOneUnit } from "./runner.js";
import { redirectUiServe } from "./ui-forward.js";

const arg = (flag: string): string | undefined => {
	const i = process.argv.indexOf(flag);
	return i >= 0 ? process.argv[i + 1] : undefined;
};

/** Every value of a repeatable flag (`--registry a=b --registry c=d`). */
const argAll = (flag: string): string[] => {
	const out: string[] = [];
	for (let i = 0; i < process.argv.length; i++) {
		if (process.argv[i] === flag && process.argv[i + 1] !== undefined) out.push(process.argv[++i]);
	}
	return out;
};

/** Flags that take a value — skipped (with their value) when collecting positionals. */
const VALUE_FLAGS = ["--plugins", "--scripts", "--registry"];

const options = {
	scriptsDir: arg("--scripts"),
	pluginsDir: arg("--plugins"),
};

/**
 * `--registry @scope=https://registry/` → `{ "@scope": "https://registry/" }`. The plugin store
 * passes one per catalog entry so a third-party package's scope resolves to *its* registry rather
 * than the public npm default.
 */
const parseRegistries = (): Record<string, string> => {
	const out: Record<string, string> = {};
	for (const spec of argAll("--registry")) {
		const eq = spec.indexOf("=");
		if (eq <= 0) {
			console.error(`[plugins] ignoring malformed --registry '${spec}' (expected @scope=URL)`);
			continue;
		}
		const scope = spec.slice(0, eq).trim();
		const url = spec.slice(eq + 1).trim();
		if (!scope.startsWith("@") || !url.startsWith("https://")) {
			console.error(
				`[plugins] ignoring --registry '${spec}' (scope must start with @, url with https://)`,
			);
			continue;
		}
		out[scope] = url;
	}
	return out;
};

// Positional plugin names after the subcommand (argv: [bun, script, <cmd>, …]). Skip flags and the
// value of `--plugins`/`--scripts` wherever they appear, so ordering doesn't matter.
const positionals = (): string[] => {
	const out: string[] = [];
	for (let i = 3; i < process.argv.length; i++) {
		const a = process.argv[i];
		if (VALUE_FLAGS.includes(a)) {
			i++; // skip its value too
			continue;
		}
		if (a.startsWith("-")) continue;
		out.push(a);
	}
	return out;
};

const pkgOpts = {
	dir: options.pluginsDir,
	// Opt-in for names that resolve on the public npm registry (supply-chain gate in
	// plugins.ts::resolvePackage). Boolean flag, so positionals() skips it on its own.
	allowPublicRegistry: process.argv.includes("--allow-public-registry"),
	// Pin the exact version in package.json instead of a caret range — the plugin store installs
	// one reviewed version and must not let a later `bun install` drift off it.
	exact: process.argv.includes("--exact"),
	registries: parseRegistries(),
};

const runPkgOp = (
	op: (names: string[], o: typeof pkgOpts) => void,
	verb: string,
): never => {
	const names = positionals();
	if (names.length === 0) {
		console.error(
			`usage: punktfunk-host plugins ${verb} <name…>  (e.g. playnite, rom-manager)`,
		);
		process.exit(2);
	}
	try {
		op(names, pkgOpts);
		process.exit(0);
	} catch (e) {
		console.error(`[plugins] ${e instanceof Error ? e.message : e}`);
		process.exit(1);
	}
};

switch (process.argv[2]) {
	case "add":
		runPkgOp(addPlugins, "add");
		break;
	case "remove":
	case "rm":
	case "uninstall":
		runPkgOp(removePlugins, "remove");
		break;
	case "list":
	case "ls": {
		const installed = listInstalled(options.pluginsDir);
		if (installed.length === 0) {
			console.log("No plugins installed.");
		} else {
			for (const p of installed) {
				console.log(p.version ? `${p.pkg}\t${p.version}` : p.pkg);
			}
		}
		process.exit(0);
	}
}

// ---- one unit, inside its sandbox (spawned by the supervisor; never run by hand) --------------
//
// The other half of `runner.ts`'s sandbox: `bwrap` re-execs this bundle with one unit to run, so a
// plugin's code gets a process — and a token — of its own. Everything it can see was decided by
// the argv on the outside; there is nothing left to enforce in here. The exit code is how the
// supervisor learns the plugin failed, and what makes it restart on the usual backoff.
const runUnit = arg("--run-unit");
if (runUnit) {
	const unit = { name: arg("--unit-name") ?? runUnit, file: runUnit };
	// Set only for a plugin without network, whose UI the supervisor forwards.
	const uiPort = Number(process.env.PUNKTFUNK_UI_PORT);
	if (uiPort > 0) redirectUiServe(uiPort);
	const unitShipper = installLogShipper();
	const unitFiber = Effect.runFork(runOneUnit(unit));
	const stopUnit = (): void => {
		void Effect.runPromise(Fiber.interrupt(unitFiber));
	};
	process.on("SIGTERM", stopUnit);
	process.on("SIGINT", stopUnit);
	const failed = await Effect.runPromise(Fiber.await(unitFiber)).then(
		(exit) => exit._tag !== "Success",
	);
	await unitShipper.flush();
	unitShipper.stop();
	process.exit(failed ? 1 : 0);
}

// ---- run the runner (default; --list keeps the legacy unit-listing behavior) ------------------
if (process.argv.includes("--list")) {
	for (const u of discoverUnits(options)) console.log(`${u.name}\t${u.file}`);
	process.exit(0);
}

// Park the process instead of spinning it. `Effect.runFork` registers NOTHING with the event
// loop — the keep-alive timer lives in `Runtime.makeRunMain`, which we deliberately don't use
// (our shutdown is the bespoke two-signal one below) — and a bun process whose only pending work
// is an unresolved promise BUSY-SPINS rather than blocking. With units running there is usually a
// socket or timer to park on, but in the documented no-op state (no scripts, no plugins) there is
// nothing at all: field report 2026-07-25 had it pinning a full core indefinitely, `strace`
// showing a bare `clock_gettime` loop and nothing else. One idle handle is the whole fix.
const keepAlive = setInterval(() => {}, 2 ** 31 - 1);

// Follow the host's REAL mgmt port before anything dials it. Plugins run in this process and
// resolve their connection from `process.env` first, so setting it here reaches every plugin —
// including one whose vendored `@punktfunk/host` predates `publishedMgmtUrl` (on Windows
// `reconcileSharedSdk` cannot refresh a read-only tree, so an old copy can outlive several host
// upgrades). An explicit PUNKTFUNK_MGMT_URL from the operator still wins.
if (!process.env.PUNKTFUNK_MGMT_URL) {
	const published = publishedMgmtUrl();
	if (published) process.env.PUNKTFUNK_MGMT_URL = published;
}

// Tee this process's output to the host so the console's Logs page can show it. Installed HERE and
// not in `runner.ts`, so it covers the supervised run only: a plugin's own CLI builds the same
// layer graph, and an operator running `punktfunk-plugin-x doctor` in their terminal is not asking
// to write into the host's log. Must be installed before the runner starts — the lines that explain
// a plugin failing to load are the first ones out.
const shipper = installLogShipper();

// Before any plugin loads: make the tree's shared SDK the one this runner was built from. A host
// upgrade is the only moment that can deliver an SDK fix to already-installed plugins, and this is
// that moment — see `reconcileSharedSdk`. Deliberately AFTER the log shipper so the operator can
// read what it did from the console's Logs page, and BEFORE `runner()` so plugins import the
// refreshed copy rather than the one they were started with.
reconcileSharedSdk(options.pluginsDir);

const fiber = Effect.runFork(runner(options));
let stopping = false;
const shutdown = (signal: string) => {
	if (stopping) return process.exit(1); // second signal = get out now
	stopping = true;
	console.log(`${new Date().toISOString()} [runner] ${signal} — interrupting units…`);
	void Effect.runPromise(Fiber.interrupt(fiber))
		// Ship what the finalizers just said before the process goes away — a clean shutdown's
		// last lines are the ones that tell you whether it WAS clean.
		.finally(() => shipper.flush())
		.finally(() => process.exit(0));
};
process.on("SIGINT", () => shutdown("SIGINT"));
process.on("SIGTERM", () => shutdown("SIGTERM"));

await Effect.runPromise(Fiber.await(fiber));
await shipper.flush(); // every unit ended on its own — don't leave their last lines unsent
shipper.stop();
clearInterval(keepAlive); // …then let the process exit
