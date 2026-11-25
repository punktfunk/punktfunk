// A network plugin's sandbox: refused without unshare or pasta, pasta's lifecycle around it, and
// how a sandbox is stopped. Shell stand-ins play bwrap and pasta, so this runs on any POSIX box.
import { afterAll, describe, expect, test } from "bun:test";
import { Effect, Exit } from "effect";
import { type ChildProcess, spawn } from "node:child_process";
import { EventEmitter } from "node:events";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { PassThrough, type Writable } from "node:stream";
import {
	connectNetwork,
	type DefaultRoutes,
	hostDefaultRoutes,
	infoPid,
	runSandboxed,
	staleRoutes,
	stopUnitChild,
} from "../src/runner-linux.js";

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

describe("stopUnitChild", () => {
	/** A child that dies on any kill, and ends on its own once stdin closes when `honours`. */
	const fake = (honours: boolean) => {
		const killed: string[] = [];
		const child = Object.assign(new EventEmitter(), {
			pid: 4242,
			exitCode: null,
			signalCode: null,
			stdin: new PassThrough(),
			kill(signal: string) {
				killed.push(signal);
				queueMicrotask(() => child.emit("exit", null, signal));
				return true;
			},
		});
		if (honours) child.stdin.on("finish", () => child.emit("exit", 1, null));
		return { child: child as unknown as ChildProcess, killed };
	};

	test("a unit that ends once its stdin closes is never killed", async () => {
		const { child, killed } = fake(true);
		await stopUnitChild(child, 50);
		await Bun.sleep(100);
		expect(killed).toEqual([]);
	});

	test("a unit that goes on running is killed once the grace is over", async () => {
		const { child, killed } = fake(false);
		const start = Date.now();
		await stopUnitChild(child, 50);
		expect(killed).toEqual(["SIGKILL"]);
		expect(Date.now() - start).toBeGreaterThanOrEqual(45);
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

	/** A sandbox released through a pasta stand-in that leaves a daemon, watched every 10 ms. */
	const released = (routes: () => DefaultRoutes, fail: (why: string) => void) => {
		const child = sandbox();
		(child.stdio[3] as Writable).end();
		const setup = pasta("route.sh", `sleep 30 & echo $! > "$f"`);
		const dir = fs.mkdtempSync(path.join(ROOT, "net-"));
		const stop = connectNetwork(child, setup, dir, fail, [], routes, 10);
		return { exited: new Promise((done) => child.on("exit", done)), stop };
	};

	test("a namespace whose IPv4 route was gone as pasta ran fails once the host has one", async () => {
		// Read before pasta, after it, then by the watch.
		const reads = [V4, NONE];
		let now = NONE;
		const why: string[] = [];
		const { exited, stop } = released(() => reads.shift() ?? now, (w) => why.push(w));
		await exited;
		await Bun.sleep(50);
		expect(why).toEqual([]);
		now = V4;
		await until(() => why.length > 0);
		await Bun.sleep(50);
		expect(why).toEqual(["the host had no default route when it started"]);
		stop();
	});

	test("a namespace connected with an IPv4 route is never watched", async () => {
		let reads = 0;
		const why: string[] = [];
		const routes = () => {
			reads++;
			return V4;
		};
		const { exited, stop } = released(routes, (w) => why.push(w));
		await exited;
		await Bun.sleep(100);
		expect(why).toEqual([]);
		expect(reads).toBe(2);
		stop();
	});
});

const NONE = { v4: false, v6: false };
const V4 = { v4: true, v6: false };
const V6 = { v4: false, v6: true };
const BOTH = { v4: true, v6: true };

describe("staleRoutes", () => {
	const NO_ROUTE = "the host had no default route when it started";
	const NO_IPV4 = "the host had no IPv4 default route when it started";

	test("a namespace without a route reconnects once the host has one of either family", () => {
		expect(staleRoutes(NONE, NONE)).toBeUndefined();
		expect(staleRoutes(NONE, V4)).toBe(NO_ROUTE);
		expect(staleRoutes(NONE, V6)).toBe(NO_ROUTE);
		expect(staleRoutes(NONE, BOTH)).toBe(NO_ROUTE);
	});

	test("a namespace with only an IPv6 route reconnects once the host has an IPv4 one", () => {
		expect(staleRoutes(V6, V6)).toBeUndefined();
		expect(staleRoutes(V6, NONE)).toBeUndefined();
		expect(staleRoutes(V6, V4)).toBe(NO_IPV4);
		expect(staleRoutes(V6, BOTH)).toBe(NO_IPV4);
	});

	test("a namespace with an IPv4 route stays, whatever the host has later", () => {
		for (const got of [V4, BOTH]) {
			for (const now of [NONE, V4, V6, BOTH]) expect(staleRoutes(got, now)).toBeUndefined();
		}
	});
});

describe("hostDefaultRoutes", () => {
	// Captured from a namespace with an address on d0 and no default route, then with one.
	const V4_HEAD = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n";
	const V4_LINK = "d0\t0009090A\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n";
	const V4_DEFAULT = "d0\t00000000\t0109090A\t0003\t0\t0\t0\t00000000\t0\t0\t0\n";
	const zero = "0".repeat(32);
	const net = `fd000009${"0".repeat(24)}`;
	const V6_REJECT = `${zero} 00 ${zero} 00 ${zero} ffffffff 00000001 00000000 00200200 lo\n`;
	const V6_LINK = `${net} 40 ${zero} 00 ${zero} 00000100 00000002 00000000 00000001 d0\n`;
	const V6_DEFAULT = `${zero} 00 ${zero} 00 ${net.slice(0, -1)}1 00000400 00000001 00000000 00000003 d0\n`;
	const proc = (v4: string, v6: string) => (file: string) => {
		if (file === "/proc/net/route") return v4;
		if (file === "/proc/net/ipv6_route") return v6;
		throw new Error(`unexpected ${file}`);
	};

	test("an address with no default route, beside the kernel's reject ::/0, is no route", () => {
		const routes = hostDefaultRoutes(proc(V4_HEAD + V4_LINK, V6_LINK + V6_REJECT));
		expect(routes).toEqual(NONE);
	});

	test("each family's default route counts for that family", () => {
		const v4 = hostDefaultRoutes(proc(V4_HEAD + V4_DEFAULT + V4_LINK, V6_REJECT));
		expect(v4).toEqual(V4);
		const v6 = hostDefaultRoutes(proc(V4_HEAD + V4_LINK, V6_LINK + V6_DEFAULT + V6_REJECT));
		expect(v6).toEqual(V6);
	});

	test("unreadable procfs is no route", () => {
		const routes = hostDefaultRoutes(() => {
			throw new Error("ENOENT");
		});
		expect(routes).toEqual(NONE);
	});
});
