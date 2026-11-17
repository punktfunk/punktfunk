// ── Recipe · VirtualHere USB passthrough → full DualSense ────────────────────────────────────
// Hand a *real* USB DualSense to the host for the length of a session, then give it back — so the
// game sees native gyro, touchpad, adaptive triggers and USB rumble instead of an emulated pad.
//
// Topology (USB-over-IP): the DualSense is plugged into the *couch* device, which runs the
// VirtualHere **Server** and shares it. The punktfunk **host** — where this script and the game
// run — runs the VirtualHere **Client** and mounts it. This script drives that client's `-t` IPC:
// `USE` the pad when a client connects, `STOP USING` it when they leave, so the couch keeps its
// own controller whenever you're not streaming.
//
// On the host you need: the VirtualHere client running (service or app) and the couch's shared
// DualSense visible to it — same-LAN setups auto-discover; otherwise add it once with
// `<VH_CLIENT> -t "MANUAL HUB ADD,<couch-ip>:7575"`.
//
//   VH_DEVICE=couch-deck.11 bun examples/virtualhere-dualsense.ts   # address from `-t LIST`
//   VH_DEVICE=DualSense     bun examples/virtualhere-dualsense.ts   # …or match by name substring
//
// To run this *outside* the repo (the normal case), drop it in its own dir, `bun add
// @punktfunk/host`, and change the import below from `../src/index.js` to `@punktfunk/host`; then
// keep it alive as a systemd user unit (its SIGTERM release hands the pad back on `systemctl
// stop`). Full walkthrough: docs → Plugin setup → VirtualHere → "Recipe: full controller
// passthrough (VirtualHere)".
//
// Env: VH_DEVICE      required — a VirtualHere address (`server.port`) or a device-name substring.
//      VH_CLIENT      client binary. Default `vhclientx86_64` (Linux); Windows `vhui64.exe`,
//                     macOS `vhclientosx`, ARM Linux `vhclientarm64`.
//      VH_ONLY_CLIENT optional — only bind for this punktfunk client label (multi-couch setups).
import { execFile } from "node:child_process";
import { connect } from "../src/index.js";

const VH_CLIENT = process.env.VH_CLIENT ?? "vhclientx86_64";
const VH_DEVICE = process.env.VH_DEVICE;
const ONLY_CLIENT = process.env.VH_ONLY_CLIENT;
if (!VH_DEVICE)
	throw new Error("set VH_DEVICE to a VirtualHere address (e.g. couch-deck.11) or a device-name substring");

// Send one command to the local VirtualHere client over its IPC channel and return the reply.
const vh = (command: string) =>
	new Promise<string>((resolve, reject) =>
		execFile(VH_CLIENT, ["-t", command], (err, stdout) => (err ? reject(err) : resolve(stdout))),
	);

// Turn a name substring into a VirtualHere address (`server.port`); pass an address straight through.
async function resolveAddress(target: string): Promise<string | null> {
	if (/^[\w.-]+\.\d+$/.test(target)) return target;
	for (const line of (await vh("LIST")).split("\n"))
		if (line.toLowerCase().includes(target.toLowerCase())) {
			const addr = line.match(/[\w.-]+\.\d+/); // the address token on the device's line
			if (addr) return addr[0];
		}
	return null;
}

const pf = await connect();
let bound: string | null = null; // the address we currently hold, so we can release exactly it
console.log(`VirtualHere passthrough ready — "${VH_DEVICE}" via ${VH_CLIENT}`);

// Bind the pad to the host as soon as a couch connects. (Prefer `stream.started`/`stream.stopped`
// instead if you'd rather pass it through only while video is actually flowing.)
pf.events.on("client.connected", async (e) => {
	if (ONLY_CLIENT && e.client.name !== ONLY_CLIENT) return;
	try {
		const addr = await resolveAddress(VH_DEVICE);
		if (!addr)
			return console.warn(`"${VH_DEVICE}" not visible to the VirtualHere client — is the couch sharing it?`);
		await vh(`USE,${addr}`);
		bound = addr;
		console.log(`bound ${addr} → ${e.client.name}`);
	} catch (err) {
		console.error("USE failed:", err);
	}
});

async function release(why: string): Promise<void> {
	if (!bound) return;
	try {
		await vh(`STOP USING,${bound}`);
		console.log(`released ${bound} (${why})`);
	} catch (err) {
		console.error("STOP USING failed:", err);
	}
	bound = null;
}

pf.events.on("client.disconnected", (e) => {
	if (ONLY_CLIENT && e.client.name !== ONLY_CLIENT) return;
	void release(`${e.client.name} left: ${e.reason}`);
});

// Clean shutdown (`systemctl stop`, ^C): hand the pad back to the couch before exiting.
for (const sig of ["SIGINT", "SIGTERM"] as const)
	process.on(sig, () => void release("runner stopping").then(() => process.exit(0)));
