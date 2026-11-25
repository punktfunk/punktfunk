// Launching a plugin on macOS: `sandbox-exec` with the plugin's Seatbelt profile (`seatbeltProfile`
// in sandbox.ts), re-executing this runner in `--run-unit` mode. Nothing is bound there, so a dir
// of the runner's own per attempt stands in for Linux's `/run/punktfunk`: the plugin's token, the
// socket the supervisor proxies to the host on, its page's socket and its temp dir (`VIEW`).
import { Effect } from "effect";
import { spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { configDir } from "./config.js";
import type { RunnerOptions, Unit } from "./runner.js";
import {
	adoptNestedState,
	exitOutcome,
	proxyToHost,
	runnerEntry,
	stopUnitChild,
	tailStderr,
	unitArgv,
	warnRefused,
	writePluginToken,
} from "./runner-linux.js";
import type { LogSink } from "./runner-log.js";
import {
	grantedPorts,
	grantedRoots,
	type PluginManifest,
	SANDBOX_EXEC,
	seatbeltEnv,
	seatbeltProfile,
	VIEW,
} from "./sandbox.js";
import { forwardUi, type UiForward } from "./ui-forward.js";

/**
 * Run one plugin in its own Seatbelt sandbox. Resolves when the child exits; a non-zero exit fails
 * the effect, which the supervisor restarts with the usual backoff. The view lives in the per-user
 * temp dir: a socket path holds 104 bytes at most, and `/var/folders/…/T` is the operator's alone.
 */
export const runSeatbelt = (
	unit: Unit,
	manifest: PluginManifest,
	options: RunnerOptions,
	log: LogSink,
): Effect.Effect<"plugin", unknown> =>
	Effect.callback<"plugin", unknown>((resume) => {
		const id = manifest.id ?? unit.name;
		const config = options.configDir ?? configDir();
		const stateDir = path.join(config, "plugin-state", id);
		adoptNestedState(stateDir, id, log);
		const home = os.homedir();
		const grants = grantedRoots(config, id);
		warnRefused(manifest, grants, home, config, id, log);
		const runtime = path.join(process.env.XDG_RUNTIME_DIR ?? os.tmpdir(), "punktfunk");
		// One per attempt: a restart's new proxy binds while the old one is still closing.
		const view = path.join(runtime, `p-${randomBytes(4).toString("hex")}`);
		let forward: UiForward | undefined;
		let proxy: { close(): void } | undefined;
		const release = (): void => {
			proxy?.close();
			forward?.close();
			fs.rmSync(view, { recursive: true, force: true });
		};
		try {
			fs.mkdirSync(runtime, { recursive: true, mode: 0o700 });
			fs.mkdirSync(path.join(view, VIEW.tmp), { recursive: true, mode: 0o700 });
			fs.mkdirSync(stateDir, { recursive: true, mode: 0o700 });
			// Where `pluginStateDir(<id>)` resolves inside, as `/run/punktfunk/plugin-state/<id>`.
			fs.mkdirSync(path.join(view, "plugin-state"));
			fs.symlinkSync(stateDir, path.join(view, "plugin-state", id));
		} catch (e) {
			release();
			resume(Effect.fail(new Error(`prepare the sandbox of ${id} in ${view}: ${e}`)));
			return;
		}
		const token = writePluginToken(config, view, id, VIEW.token);
		if (token instanceof Error) {
			release();
			resume(Effect.fail(token));
			return;
		}
		proxy = proxyToHost(path.join(view, VIEW.socket), options);
		try {
			forward = forwardUi(path.join(view, VIEW.ui));
		} catch (e) {
			log(`[runner] ${id}: its settings page stays unreachable — ${e}`, "warn");
		}
		const profile = seatbeltProfile(
			manifest,
			{
				stateDir,
				view,
				pluginsDir: options.pluginsDir ?? path.join(config, "plugins"),
				bun: process.execPath,
				runner: runnerEntry(),
				home,
				configDir: config,
				ui: forward !== undefined,
			},
			grants,
			manifest.network ? grantedPorts(config, id) : [],
		);
		const args = ["-p", profile, process.execPath, ...unitArgv(unit), "--stop-on-eof"];
		const child = spawn(SANDBOX_EXEC, args, {
			env: seatbeltEnv(home, view, forward?.port),
			cwd: stateDir,
			// stdin is the stop channel (`stopUnitChild`).
			stdio: ["pipe", "inherit", "pipe"],
		});
		// sandbox-exec's own refusal comes before the plugin can ship a log line.
		const lastLines = tailStderr(child);
		child.on("error", (e) => {
			release();
			resume(Effect.fail(e));
		});
		child.on("exit", (code, signal) => {
			release();
			resume(exitOutcome("sandboxed plugin", code, signal, lastLines()));
		});
		// Interruption (shutdown, a grant change) waits for the plugin's finalizers, its view and
		// host proxy still there.
		return Effect.promise(() => stopUnitChild(child).then(release));
	});
