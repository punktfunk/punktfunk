// The update predicates the panel branches on: which pending update shows at all, which client
// update gets a button, and which only gets the command. Run by `pnpm test`.
import assert from "node:assert/strict";
import type { UpdateInfo } from "../src/backend.ts";
import { clientUpdateIsManualOnly, clientUpdateIsOneTap, hasUpdate } from "../src/update/model.ts";

const info = (o: Partial<UpdateInfo>): UpdateInfo => ({
  current: "0.30.0",
  latest: "0.30.0",
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

for (const none of [null, undefined]) {
  assert.equal(hasUpdate(none), false);
  assert.equal(clientUpdateIsOneTap(none), false);
  assert.equal(clientUpdateIsManualOnly(none), false);
}

assert.equal(hasUpdate(info({})), false, "nothing pending");
assert.equal(hasUpdate(info({ update_available: true })), true, "plugin only");
assert.equal(hasUpdate(info({ client_update_available: true })), true, "client only");

// [applier, client update pending, one tap, manual only]
const cases: [string, boolean, boolean, boolean][] = [
  ["flatpak", true, true, false],
  ["helper", true, true, false],
  ["none", true, false, true],
  ["", true, false, true],
  ["flatpak", false, false, false],
  ["none", false, false, false],
];
for (const [applier, pending, oneTap, manual] of cases) {
  const i = info({ client_applier: applier, client_update_available: pending });
  assert.equal(clientUpdateIsOneTap(i), oneTap, `one tap: ${applier} pending=${pending}`);
  assert.equal(clientUpdateIsManualOnly(i), manual, `manual only: ${applier} pending=${pending}`);
}
// A pending plugin update never makes the client one-tap.
assert.equal(clientUpdateIsOneTap(info({ update_available: true, client_applier: "flatpak" })), false);

console.log(`update: ${cases.length + 6} cases pass`);
