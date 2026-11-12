// The Quick Access panel's decisions: which hosts stream on a press and which need access, what a
// row says about itself, when it shows its address and its ▾, which update row shows, what "Check
// for updates" reports, and when the Streaming section is up. Run by `pnpm test`.
import assert from "node:assert/strict";
import type { UpdateInfo } from "../src/backend.ts";
import { type HostView, needsPair, splitByAccess } from "../src/hosts/model.ts";
import { rowMenu, rowStatus, sharedNames, streamIsUp } from "../src/panel/model.ts";
import { checkOutcome, updateBanner } from "../src/update/model.ts";

const host = (o: Partial<HostView>): HostView => ({
  name: "desk",
  addr: "192.168.1.20",
  port: 47989,
  fp: "ab".repeat(32),
  advertisedFp: "",
  moved: false,
  paired: true,
  online: true,
  wakeable: false,
  saved: true,
  pairPolicy: "",
  os: "",
  ref: "id-desk",
  preset: null,
  pinnedPresets: [],
  lastUsed: null,
  ...o,
});

// The split: a pinned fingerprint streams, paired or not; no pin opens the trust sheet. List order
// survives inside each group.
const paired = host({ name: "a", ref: "a" });
const trusted = host({ name: "b", ref: "b", paired: false });
const found = host({ name: "c", ref: "c", fp: "", paired: false, saved: false, advertisedFp: "cd".repeat(32) });
const byAddress = host({ name: "d", ref: "d", fp: "", paired: false, online: false });
const views = [paired, found, trusted, byAddress];
const { ready, access } = splitByAccess(views);
assert.deepEqual(ready, [paired, trusted], "ready keeps pinned hosts in order");
assert.deepEqual(access, [found, byAddress], "access keeps pinless hosts in order");
for (const v of views) {
  assert.equal(access.includes(v), needsPair(v), `${v.name}: split agrees with needsPair`);
}
assert.deepEqual(splitByAccess([]), { ready: [], access: [] });

// Status: plain online says nothing extra; offline says whether the launch can wake it.
assert.equal(rowStatus(host({ online: true, wakeable: true })), "online");
assert.equal(rowStatus(host({ online: false, wakeable: true })), "offline-wakes");
assert.equal(rowStatus(host({ online: false, wakeable: false })), "offline");

// The address shows only on rows that share a name, across both groups.
const twins = [host({ name: "desk", ref: "1" }), host({ name: "desk", ref: "2", fp: "" }), host({ name: "tv", ref: "3" })];
assert.deepEqual([...sharedNames(twins)], ["desk"]);
assert.equal(sharedNames([host({ name: "desk" }), host({ name: "Desk" })]).size, 0, "names compare exactly");
assert.equal(sharedNames([]).size, 0);

// The ▾: pinned presets, or two profiles and up.
assert.equal(rowMenu(0, 0), null);
assert.equal(rowMenu(0, 1), null, "a single profile is no choice");
assert.deepEqual(rowMenu(0, 2), { profiles: true, presets: false });
assert.deepEqual(rowMenu(1, 1), { profiles: false, presets: true });
assert.deepEqual(rowMenu(2, 3), { profiles: true, presets: true });

// Streaming is up on either arm: the socket, or our shortcut still waking its host.
assert.equal(streamIsUp(false, false), false);
assert.equal(streamIsUp(true, false), true);
assert.equal(streamIsUp(false, true), true, "a launch still waking its host offers Stop");
assert.equal(streamIsUp(true, true), true);

const info = (o: Partial<UpdateInfo>): UpdateInfo => ({
  current: "0.30.0",
  latest: "0.31.0",
  artifact: "",
  hash: "",
  channel: "latest",
  update_available: false,
  client_update_available: false,
  client_current: "",
  client_latest: "",
  client_install: "",
  client_applier: "none",
  client_command: "",
  client_opt_in: "",
  ...o,
});

// Every update row.
assert.equal(updateBanner(null), "none");
assert.equal(updateBanner(info({})), "none");
assert.equal(updateBanner(info({ client_update_available: true, client_applier: "none" })), "manual-client");
assert.equal(updateBanner(info({ client_update_available: true, client_applier: "flatpak" })), "client");
assert.equal(updateBanner(info({ client_update_available: true, client_applier: "helper" })), "client");
assert.equal(updateBanner(info({ update_available: true })), "plugin");
assert.equal(
  updateBanner(info({ update_available: true, client_update_available: true, client_applier: "none" })),
  "plugin",
  "a manual-only client never rides the button's promise",
);
assert.equal(
  updateBanner(info({ update_available: true, client_update_available: true, client_applier: "flatpak" })),
  "plugin-and-client",
);

// Every check outcome, and when the client needs a hand install.
const outcomes: [UpdateInfo | null, string, boolean][] = [
  [null, "unreachable", false],
  [info({ error: "fetch-failed" }), "unreachable", false],
  [info({ update_available: true }), "plugin", false],
  [info({ update_available: true, client_update_available: true, client_latest: "0.31.0", client_applier: "flatpak" }), "both", false],
  [info({ update_available: true, client_update_available: true, client_applier: "flatpak" }), "both-unknown", false],
  [info({ update_available: true, client_update_available: true, client_latest: "0.31.0" }), "both", true],
  [info({ client_update_available: true, client_latest: "0.31.0", client_applier: "helper" }), "client", false],
  [info({ client_update_available: true, client_applier: "flatpak" }), "client-unknown", false],
  [info({ client_update_available: true, client_latest: "0.31.0" }), "client", true],
  [info({ client_error: "client-outdated" }), "client-outdated", false],
  [info({ client_error: "fetch-failed" }), "client-check-failed", false],
  [info({ client_error: "no-origin", error: "update-channel-unknown" }), "client-check-failed", false],
  [info({ error: "update-channel-unknown" }), "dev-build", false],
  [info({}), "up-to-date", false],
];
for (const [res, kind, manual] of outcomes) {
  assert.deepEqual(checkOutcome(res), { kind, manual }, JSON.stringify(res));
}
// A failed client check never reads as up to date, whatever else the answer says.
for (const client_error of ["client-outdated", "client-unavailable", "no-origin", "fetch-failed"]) {
  assert.notEqual(checkOutcome(info({ client_error })).kind, "up-to-date", client_error);
}

console.log(`panel: split, rows, update rows and ${outcomes.length} check outcomes pass`);
