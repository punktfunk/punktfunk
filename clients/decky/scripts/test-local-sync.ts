// The sync of this device's library into Steam, as data: which host answers count, which titles
// are wanted, added, kept and removed under each removal, the collection each is filed in, the
// titles that share a stream's layout, and the map the host gets. Steam itself is not here: what
// Steam does with the calls is only seen on a Deck. Run by `pnpm test`.
import assert from "node:assert/strict";
import type { LocalEntriesResult, LocalEntry } from "../src/backend.ts";
import {
  collectionFor,
  goneIds,
  type LocalAnswer,
  plan,
  readAnswer,
  sharedLayout,
  steamMap,
  wanted,
} from "../src/steam/local-plan.ts";

const entry = (id: string, source: string, o: Partial<LocalEntry> = {}): LocalEntry => ({
  id,
  title: id.split(/[:/]/).pop() ?? id,
  source,
  art: [],
  ...o,
});

const raw: LocalEntriesResult = {
  ok: true,
  steam_owns_desk: true,
  sources: [
    { id: "rom-manager", name: "ROM Manager", games: 3 },
    { id: "heroic", name: "Heroic", games: 1 },
  ],
  entries: [
    entry("rom-manager:snes/chrono", "rom-manager", { title: "Chrono Trigger", platform: "SNES" }),
    entry("rom-manager:gba/minish", "rom-manager", { title: "Minish Cap", platform: "GBA" }),
    entry("rom-manager:snes/dkc", "rom-manager", { title: "Donkey Kong Country", platform: " " }),
    entry("heroic:Dopeki", "heroic", { title: "Dopeki", art: ["portrait", "hero", 3 as unknown as string] }),
  ],
};

// The answer counts only when the host answered and Steam owns the desk.
assert.equal(readAnswer(null), null, "no answer");
assert.equal(readAnswer({ ok: false, error: "host-unavailable" }), null, "host down");
assert.equal(readAnswer({ ...raw, steam_owns_desk: false }), null, "a desktop owns the desk");
assert.equal(readAnswer({ ...raw, steam_owns_desk: undefined }), null, "a host too old to say");
const answer = readAnswer(raw) as LocalAnswer;
assert.equal(answer.entries.length, 4);
assert.deepEqual(answer.entries[3].art, ["portrait", "hero"], "art kinds are strings");
// An entry without an id, a title or a source never becomes a Steam entry.
const junk = readAnswer({
  ...raw,
  sources: [{ id: "", name: "x", games: 0 }, { id: "a", name: "", games: 0 }],
  entries: [
    entry("", "rom-manager"),
    entry("x", "rom-manager", { title: "  " }),
    entry("y", ""),
    { id: "z", title: "Z", source: "a" } as LocalEntry,
  ],
}) as LocalAnswer;
assert.deepEqual(junk.entries.map((e) => e.id), ["z"], "malformed entries dropped");
assert.deepEqual(junk.entries[0].art, [], "missing art reads as none");
assert.deepEqual(junk.sources, [{ id: "a", name: "a", games: 0 }], "a nameless source shows its id");

// Wanted: the titles of the sources switched on, nothing else.
const romOn = new Set(["rom-manager"]);
assert.deepEqual(wanted(answer, new Set()).length, 0, "off by default");
assert.deepEqual(
  wanted(answer, romOn).map((e) => e.id),
  ["rom-manager:snes/chrono", "rom-manager:gba/minish", "rom-manager:snes/dkc"],
);

// The plan. Chrono has a live entry, Minish a dead one, DKC none; Dopeki's source is off; one
// recorded title left the library.
const have: Array<[string, number]> = [
  ["rom-manager:snes/chrono", 3001],
  ["rom-manager:gba/minish", 3002],
  ["heroic:Dopeki", 3003],
  ["rom-manager:snes/gone", 3004],
];
const alive = (appId: number) => (appId === 3002 ? false : appId === 3005 ? null : true);
const atLoad = plan(answer, romOn, have, alive, "none");
assert.deepEqual(atLoad.add.map((e) => e.id), ["rom-manager:gba/minish", "rom-manager:snes/dkc"], "add = want − alive have");
assert.deepEqual(atLoad.keep.map(([e, a]) => [e.id, a]), [["rom-manager:snes/chrono", 3001]]);
assert.deepEqual(atLoad.drop, [], "never a removal at load");
assert.deepEqual(plan(answer, romOn, have, alive, "off").drop, [["heroic:Dopeki", 3003]], "the panel takes a switched-off source");
assert.deepEqual(
  plan(answer, romOn, have, alive, "all").drop,
  [["heroic:Dopeki", 3003], ["rom-manager:snes/gone", 3004]],
  "Remove also takes a title that left the library",
);
// An appId Steam can't vouch for is added again, which reuses it once Steam answers.
const unsure = plan(answer, romOn, [["rom-manager:snes/chrono", 3005]], alive, "none");
assert.deepEqual(unsure.add.map((e) => e.id), ["rom-manager:snes/chrono", "rom-manager:gba/minish", "rom-manager:snes/dkc"]);
// Only entries on record are ever removed: a source switched off with nothing recorded drops nothing.
assert.deepEqual(plan(answer, new Set(), [], alive, "all").drop, []);
// Switching everything off removes every listed entry on record, and nothing more.
assert.deepEqual(
  plan(answer, new Set(), have, alive, "off").drop.map(([id]) => id),
  ["rom-manager:snes/chrono", "rom-manager:gba/minish", "heroic:Dopeki"],
);
assert.deepEqual(goneIds(answer, have), ["rom-manager:snes/gone"]);
assert.deepEqual(goneIds(answer, []), []);

// Two titles of one name stay two entries of that name.
const twins = readAnswer({
  ...raw,
  entries: [
    entry("rom-manager:snes/tetris", "rom-manager", { title: "Tetris", platform: "SNES" }),
    entry("rom-manager:gb/tetris", "rom-manager", { title: "Tetris", platform: "Game Boy" }),
  ],
}) as LocalAnswer;
const twinPlan = plan(twins, romOn, [], alive, "none");
assert.deepEqual(twinPlan.add.map((e) => e.title), ["Tetris", "Tetris"], "same name, no suffix");
assert.deepEqual(
  twinPlan.add.map((e) => collectionFor(e, twins.sources)),
  ["SNES", "Game Boy"],
  "told apart by their collections",
);

// The collection: the platform as the library has it, else the source's name, else its id.
const [chrono, minish, dkc, dopeki] = answer.entries;
assert.equal(collectionFor(chrono, answer.sources), "SNES");
assert.equal(collectionFor(minish, answer.sources), "GBA");
assert.equal(collectionFor(dkc, answer.sources), "ROM Manager", "a blank platform falls to the source");
assert.equal(collectionFor(dopeki, answer.sources), "Heroic");
assert.equal(collectionFor(entry("x:y", "lutris"), answer.sources), "lutris", "an unlisted source by its id");
assert.equal(collectionFor({ ...chrono, platform: " PS2 " }, answer.sources), "PS2");

// A title whose lowercase name has a stream layout marker shares that layout.
assert.deepEqual(sharedLayout(answer.entries, new Set(["chrono trigger", "dota 2"])), ["Chrono Trigger"]);
assert.deepEqual(sharedLayout(twins.entries, new Set(["tetris"])), ["Tetris"], "listed once per name");
assert.deepEqual(sharedLayout(answer.entries, new Set()), []);

// The map: every synced entry on record, and every other entry the plugin made, once each.
assert.deepEqual(steamMap(have, [2764001122, null, 2764001123, 2764001122, 0]), {
  version: 1,
  entries: {
    "rom-manager:snes/chrono": 3001,
    "rom-manager:gba/minish": 3002,
    "heroic:Dopeki": 3003,
    "rom-manager:snes/gone": 3004,
  },
  own: [2764001122, 2764001123],
});
assert.deepEqual(steamMap([], []), { version: 1, entries: {}, own: [] });

console.log("local sync: answers, plans, collections, shared layouts and the map pass");
