// A network plugin's sandbox: refused without unshare or pasta, and pasta's lifecycle around it.
// Shell stand-ins play bwrap and pasta, so this runs on any POSIX box.
import { afterAll, describe, expect, test } from "bun:test";
import { Effect, Exit } from "effect";
import { spawn } from "node:child_process";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type { Writable } from "node:stream";
import { connectNetwork, infoPid, runSandboxed } from "../src/runner-linux.js";

const ROOT = fs.mkdtempSync(path.join(os.tmpdir(), "pf-runner-linux-"));
afterAll(() => fs.rmSync(ROOT, { recursive: true, force: true }));

const until = async (done: () => boolean, ms = 5000): Promise<void> => {
	for (const end = Date.now() + ms; !done(); ) {
		if (Date.now() > end) throw new Error("timed out");
		await Bun.sleep(20);
	}
};
/** Still running. A killed orphan stays a zombie where pid 1 never reaps, as in a container. */
const alive = (pid: number): boolean => {
	try {
		process.kill(pid, 0);
	} catch {
		return false;
	}
	try {
		const stat = fs.readFileSync(`/proc/${pid}/stat`, "utf8");
		return stat[stat.lastIndexOf(")") + 2] !== "Z";
	} catch {
		// No /proc here: the signal's answer stands.
		return true;
	}
};

describe("runSandboxed", () => {
	const unit = { name: "demo", file: path.join(ROOT, "index.ts") };
	const refusal = async (network: boolean, found: string[]) => {
		const asked: string[] = [];
		const find = (cmd: string) => {
			asked.push(cmd);
			return found.includes(cmd) ? `/usr/bin/${cmd}` : undefined;
		};
		const exit = await Effect.runPromiseExit(
			runSandboxed(unit, { schema: 1, id: "demo", network }, { configDir: ROOT }, () => {}, find),
		);
		return { asked, why: Exit.isFailure(exit) ? String(exit.cause) : "" };
	};

	test("refuses a network plugin without unshare or pasta, and nothing else", async () => {
		expect((await refusal(true, ["unshare"])).why).toContain("pasta from passt is not on PATH");
		expect((await refusal(true, ["pasta"])).why).toContain("unshare from util-linux is not on PATH");
		// A plugin without network never looks: it goes on to its token, which this box lacks.
		const plain = await refusal(false, []);
		expect(plain.asked).toEqual([]);
		expect(plain.why).toContain("No API credential exists for demo");
	});
});

test("infoPid waits for the character after the number", () => {
	expect(infoPid('{\n    "child-pid": 12')).toBeUndefined();
	expect(infoPid('{\n    "child-pid": 1234,\n')).toBe(1234);
});

describe("connectNetwork", () => {
	/** A bwrap stand-in: reports its own pid on fd 4, then exits 0 only once fd 5 reads "1". */
	const sandbox = () =>
		spawn("/bin/sh", ["-c", `printf '{"child-pid": %s,\\n' $$ >&4; read -r go <&5; [ "$go" = 1 ]`], {
			stdio: ["ignore", "inherit", "inherit", "pipe", "pipe", "pipe"],
		});
	/** A pasta stand-in: `body` runs with $f the pid file and $target the pid it was given. */
	const pasta = (name: string, body: string): string => {
		const file = path.join(ROOT, name);
		fs.writeFileSync(
			file,
			`#!/bin/sh\nwhile [ $# -gt 1 ]; do [ "$1" = --pid ] && f=$2; shift; done\ntarget=$1\n${body}\n`,
			{ mode: 0o755 },
		);
		return file;
	};

	test("releases the sandbox once pasta is up, and stops the daemon after it", async () => {
		const dir = fs.mkdtempSync(path.join(ROOT, "net-"));
		const child = sandbox();
		(child.stdio[3] as Writable).end();
		const setup = pasta("up.sh", `sleep 30 & echo $! > "$f"; echo "$target" > "$f.target"`);
		const stop = connectNetwork(child, setup, dir, (why) => {
			throw new Error(why);
		});
		const code = await new Promise((done) => child.on("exit", done));
		expect(code).toBe(0);
		// pasta joins the holder bwrap runs in, which is the spawned pid itself.
		expect(fs.readFileSync(path.join(dir, "pasta.pid.target"), "utf8").trim()).toBe(String(child.pid));
		const daemon = Number(fs.readFileSync(path.join(dir, "pasta.pid"), "utf8"));
		expect(alive(daemon)).toBe(true);
		stop();
		await until(() => !alive(daemon));
		expect(fs.existsSync(dir)).toBe(false);
	});

	test("a stop during setup kills the blocked sandbox and the daemon the setup leaves", async () => {
		const dir = fs.mkdtempSync(path.join(ROOT, "net-"));
		const child = sandbox();
		(child.stdio[3] as Writable).end();
		const marker = path.join(ROOT, "setup.pid");
		// Up only once told to stop, as a setup that finishes after the release.
		const setup = pasta(
			"slow.sh",
			`trap 'sleep 30 & echo $! > "$f"; echo $! > "${marker}.daemon"; exit 0' TERM\necho $$ > "${marker}"\nwhile :; do sleep 0.05; done`,
		);
		const exited = new Promise((done) => child.on("exit", (_code, signal) => done(signal)));
		const stop = connectNetwork(child, setup, dir, (why) => {
			throw new Error(why);
		});
		await until(() => fs.existsSync(marker));
		stop();
		expect(await exited).toBe("SIGKILL");
		await until(() => fs.existsSync(`${marker}.daemon`));
		const daemon = Number(fs.readFileSync(`${marker}.daemon`, "utf8"));
		await until(() => !alive(daemon) && !fs.existsSync(dir));
	});
});
