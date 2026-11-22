// Replays clients/shared/host-row-vectors.json through mergeHosts: which advert each saved host
// matches (fingerprint when both carry one, else addr:port) and which adverts stay rows of their
// own. Only the match is compared; the console's other row fields are its own. Run by `pnpm test`.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import type { DiscoveredHost, SavedHost } from "../src/backend.ts";
import { hostRef, mergeHosts } from "../src/hosts/model.ts";

interface Vectors {
  cases: {
    name: string;
    saved: {
      id: string;
      name: string;
      addr: string;
      port: number;
      fp: string;
      mac: string[];
      os: string;
      last_used: number | null;
    }[];
    discovered: {
      name: string;
      addr: string;
      port: number;
      fp: string;
      os: string;
      mgmt_port: number | null;
    }[];
    online: string[];
    rows: { key: string; saved: boolean; os: string; pin: string | null }[];
  }[];
}

interface Row {
  key: string;
  saved: boolean;
  os: string;
}

const file = new URL("../../shared/host-row-vectors.json", import.meta.url);
const { cases } = JSON.parse(readFileSync(file, "utf8")) as Vectors;
assert.ok(cases.length > 0, "no cases in host-row-vectors.json");
const byKey = (a: Row, b: Row) => a.key.localeCompare(b.key);

for (const c of cases) {
  const saved: SavedHost[] = c.saved.map((s) => ({
    id: s.id,
    name: s.name,
    addr: s.addr,
    port: s.port,
    fp_hex: s.fp,
    paired: false,
    mac: s.mac,
    os: s.os,
    last_used: s.last_used,
    clipboard_sync: false,
    online: c.online.includes(s.id),
  }));
  // No `saved_id`: the CLI's own match is absent, so the fallback rule decides.
  const discovered: DiscoveredHost[] = c.discovered.map((a) => ({
    name: a.name,
    addr: a.addr,
    port: a.port,
    fp: a.fp,
    pair: "",
    id: "",
    mgmt: a.mgmt_port ?? 0,
    os: a.os,
    saved: false,
    paired: false,
  }));
  const got: Row[] = mergeHosts(saved, discovered).map((v) => ({
    key: (v.saved ? v.fp : v.advertisedFp) || `${v.addr}:${v.port}`,
    saved: v.saved,
    os: v.os,
  }));
  // A preset card is the console's own row; Decky nests it under its host.
  const want: Row[] = c.rows
    .filter((r) => r.pin === null)
    .map(({ key, saved, os }) => ({ key, saved, os }));
  assert.deepEqual(got.sort(byKey), want.sort(byKey), c.name);
}
console.log(`host-row-vectors: ${cases.length} cases match`);

// The CLI reads a bare IPv6 address's last group as the port, so the ref brackets it.
assert.equal(hostRef("200:abcd::1", 9777), "[200:abcd::1]:9777");
assert.equal(hostRef("[200:abcd::1]", 9777), "[200:abcd::1]:9777");
assert.equal(hostRef("192.168.1.9", 9777), "192.168.1.9:9777");
assert.equal(hostRef("desk.lan", 9800), "desk.lan:9800");
const advert = { name: "Desk", addr: "fd00::9", port: 9777, fp: "", pair: "", id: "", mgmt: 0, os: "" };
const [unsaved] = mergeHosts([], [{ ...advert, saved: false, paired: false }]);
assert.equal(unsaved.ref, "[fd00::9]:9777");
console.log("host refs: an IPv6 address is bracketed");
