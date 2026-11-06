// Drives pinNativeRuntime against a fake SteamClient.Apps: a shortcut Steam would run in a tool
// is pinned to the native runtime, and one it runs in none keeps today's launch. Run by `pnpm test`.
import assert from "node:assert/strict";
import { type CompatApps, NATIVE_TOOL, pinNativeRuntime } from "../src/compat.ts";

type Details = { strCompatToolName?: string } | null;

interface FakeOpts {
  sync?: boolean; // details arrive inside RegisterForAppDetails, before it returns
  nullFirst?: boolean; // an empty push precedes the real one
  answers?: boolean; // false: Steam never pushes details
}

/** A fake Apps whose effective tool is `tool` (undefined = field absent). */
function fakeApps(tool: string | undefined, o: FakeOpts = {}) {
  const specified: string[] = [];
  let registered = 0;
  let unregistered = 0;
  const apps: CompatApps = {
    SpecifyCompatTool: (_appId, t) => void specified.push(t),
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
  return { apps, specified, balanced: () => registered === 1 && unregistered === 1 };
}

const cases: [string, string | undefined, FakeOpts, string[], string | null][] = [
  ["inherited global default", "proton_11", {}, [NATIVE_TOOL], "proton_11"],
  ["answer before registration returns", "proton_experimental", { sync: true }, [NATIVE_TOOL], "proton_experimental"],
  ["runtime container, empty push first", "steamlinuxruntime_sniper", { nullFirst: true }, [NATIVE_TOOL], "steamlinuxruntime_sniper"],
  ["no tool", "", {}, [], null],
  ["field absent", undefined, {}, [], null],
  ["already native", NATIVE_TOOL, {}, [], null],
  ["Steam never answers", "proton_experimental", { answers: false }, [], null],
];

for (const [name, tool, opts, wantSpecified, wantReplaced] of cases) {
  const f = fakeApps(tool, opts);
  assert.equal(await pinNativeRuntime(f.apps, 1, 50), wantReplaced, name);
  assert.deepEqual(f.specified, wantSpecified, name);
  assert.ok(f.balanced(), `${name}: registers once and unregisters once`);
}

// A Steam without the binding, or one that throws, is left alone, as with no tool.
const specified: string[] = [];
const bare: CompatApps = { SpecifyCompatTool: (_a, t) => void specified.push(t) };
assert.equal(await pinNativeRuntime(bare, 1, 20), null);
const throwing: CompatApps = {
  ...bare,
  RegisterForAppDetails: () => {
    throw new Error("gone");
  },
};
assert.equal(await pinNativeRuntime(throwing, 1, 20), null);
assert.deepEqual(specified, []);

console.log(`compat: ${cases.length + 2} cases pass`);
