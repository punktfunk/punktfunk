// Drives compatToolOf against a fake SteamClient.Apps: the tool Steam reports, "" for none, null
// when Steam does not answer, and exactly one unregister per registration. Run by `pnpm test`.
import assert from "node:assert/strict";
import { type CompatApps, compatToolOf } from "../src/compat.ts";

type Details = { strCompatToolName?: string } | null;

interface FakeOpts {
  sync?: boolean; // details arrive inside RegisterForAppDetails, before it returns
  nullFirst?: boolean; // an empty push precedes the real one
  answers?: boolean; // false: Steam never pushes details
}

/** A fake Apps whose effective tool is `tool` (undefined = field absent). */
function fakeApps(tool: string | undefined, o: FakeOpts = {}) {
  let registered = 0;
  let unregistered = 0;
  const apps: CompatApps = {
    RegisterForAppDetails: (_appId, cb) => {
      registered++;
      const send = () => {
        if (o.answers === false) {
          return;
        }
        if (o.nullFirst) {
          cb(null);
        }
        cb(tool === undefined ? {} : { strCompatToolName: tool });
      };
      o.sync ? send() : setTimeout(send, 0);
      return { unregister: () => void unregistered++ };
    },
  };
  return { apps, balanced: () => registered === 1 && unregistered === 1 };
}

const cases: [string, string | undefined, FakeOpts, string | null][] = [
  ["inherited global default", "proton_11", {}, "proton_11"],
  ["answer before registration returns", "proton_experimental", { sync: true }, "proton_experimental"],
  ["runtime container, empty push first", "steamlinuxruntime_sniper", { nullFirst: true }, "steamlinuxruntime_sniper"],
  ["no tool", "", {}, ""],
  ["field absent", undefined, {}, ""],
  ["Steam never answers", "proton_experimental", { answers: false }, null],
];

for (const [name, tool, opts, want] of cases) {
  const f = fakeApps(tool, opts);
  assert.equal(await compatToolOf(f.apps, 1, 50), want, name);
  assert.ok(f.balanced(), `${name}: registers once and unregisters once`);
}

// A Steam without the binding, or one that throws, answers null.
assert.equal(await compatToolOf({}, 1, 20), null);
const throwing: CompatApps = {
  RegisterForAppDetails: () => {
    throw new Error("gone");
  },
};
assert.equal(await compatToolOf(throwing, 1, 20), null);

console.log(`compat: ${cases.length + 2} cases pass`);
