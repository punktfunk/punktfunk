// The shipped entry point's idle contract: `punktfunk-scripting` with nothing to run must PARK,
// not spin. `Effect.runFork` registers nothing with the event loop (the keep-alive lives in
// `Runtime.makeRunMain`, which runner-cli deliberately doesn't use), and a bun process whose only
// pending work is an unresolved promise busy-loops instead of blocking — so in the runner's own
// documented no-op state (no scripts, no plugins) it would pin a full core without the idle handle
// runner-cli holds (`keepAlive`). Measured against the real CLI rather than mocked: parking is a
// property of the process, not of any function in it.
import { afterAll, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as path from "node:path";
import { SANDBOX_REFUSED_EXIT } from "../src/runner.js";

const SDK = path.join(import.meta.dir, "..");
const ROOT = path.join(SDK, `.runner-cli-fixtures-${process.pid}`);
const SCRIPTS = path.join(ROOT, "scripts");
const PLUGINS = path.join(ROOT, "plugins");
fs.mkdirSync(SCRIPTS, { recursive: true });
fs.mkdirSync(PLUGINS, { recursive: true });
afterAll(() => fs.rmSync(ROOT, { recursive: true, force: true }));

// Long enough that bun's startup (~0.3 s of CPU, and it does not grow with the window) is a small
// share of what we measure.
const WINDOW_MS = 3_000;
// A spinning runner burns a whole core — one window's worth of CPU in one window. A parked one
// burns its startup and nothing after. Half the window sits far from both, so a contended CI
// runner that only lets the spinner have half a core still fails.
const MAX_CPU_MS = WINDOW_MS / 2;

test(
	"an idle runner parks instead of spinning",
	async () => {
		const proc = Bun.spawn(
			[
				process.execPath,
				"src/runner-cli.ts",
				"--scripts",
				SCRIPTS,
				"--plugins",
				PLUGINS,
			],
			{ cwd: SDK, stdout: "pipe", stderr: "pipe" },
		);
		await Bun.sleep(WINDOW_MS);
		proc.kill("SIGTERM");
		await proc.exited;
		const out = await new Response(proc.stdout).text();

		// A process that died on startup also burns no CPU — assert it really ran the runner and
		// was still alive to catch the signal, so the CPU bound below can't pass vacuously.
		expect(out).toContain("[runner] nothing to run");
		expect(out).toContain("[runner] SIGTERM");

		const cpuMs = Number(proc.resourceUsage()?.cpuTime.total ?? 0) / 1000;
		expect(cpuMs).toBeLessThan(MAX_CPU_MS);
	},
	WINDOW_MS + 15_000,
);

// The refusal's exit status is the contract with every supervisor (`RestartPreventExitStatus=78`
// in the unit), so it is read off the real process. Linux only: that is where the default is a
// sandbox and the probe is a program on PATH.
test.skipIf(process.platform !== "linux")(
	"a plugin on a box with no bwrap ends the runner with the refusal status",
	async () => {
		const root = path.join(ROOT, "refused");
		const pkg = path.join(root, "plugins", "node_modules", "punktfunk-plugin-demo");
		for (const dir of [pkg, path.join(root, "scripts"), path.join(root, "no-bwrap")])
			fs.mkdirSync(dir, { recursive: true });
		const files = {
			"package.json": JSON.stringify({
				name: "punktfunk-plugin-demo",
				main: "index.js",
				punktfunk: { schema: 1, id: "demo" },
			}),
			"index.js": "export default {};",
		};
		for (const [name, text] of Object.entries(files)) {
			fs.writeFileSync(path.join(pkg, name), text);
			fs.chmodSync(path.join(pkg, name), 0o644);
		}
		const proc = Bun.spawn(
			[
				process.execPath,
				"src/runner-cli.ts",
				"--scripts",
				path.join(root, "scripts"),
				"--plugins",
				path.join(root, "plugins"),
			],
			{
				cwd: SDK,
				// An empty PATH dir: the probe can't pass, whatever this machine has installed.
				env: {
					...process.env,
					PATH: path.join(root, "no-bwrap"),
					PUNKTFUNK_CONFIG_DIR: root,
					PUNKTFUNK_PLUGIN_SANDBOX: "on",
				},
				stdout: "pipe",
				stderr: "pipe",
			},
		);
		expect(await proc.exited).toBe(SANDBOX_REFUSED_EXIT);
		expect(await new Response(proc.stderr).text()).toContain(
			"stopping instead of running punktfunk-plugin-demo",
		);
		expect(fs.existsSync(path.join(root, "plugin-state", "runner.refused"))).toBe(true);
	},
	20_000,
);
