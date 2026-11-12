// What may ride Steam's launch options: every id shape the clients really use passes, and every
// shell metacharacter, space, flag lead and over-long value is refused. Then the options string
// itself: the token order the wrapper reads. Run by `pnpm test`.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  isSafeClientBin,
  isSafeLaunchId,
  launchOptions,
  validateLaunch,
} from "../src/steam/launch-options.ts";

const shared = (name: string) =>
  JSON.parse(readFileSync(new URL(`../../shared/${name}`, import.meta.url), "utf8"));

// The launch ids and host refs the deep-link vectors expect to parse.
const fromVectors: string[] = [];
for (const c of shared("deeplink-vectors.json").cases) {
  if (c.error === undefined && !/\s/.test(c.expect?.host_ref ?? "")) {
    fromVectors.push(c.expect.host_ref);
  }
  if (c.error === undefined && c.expect?.launch) {
    fromVectors.push(c.expect.launch);
  }
}
// Library ids a Steam game launch can carry (`steam:<appid>`), and a hex custom id.
const fromLibrary = shared("library-id-vectors.json")
  .entry_ids.filter((e: { value: string; valid: boolean; fill?: string }) => e.valid && !e.fill)
  .map((e: { value: string }) => e.value)
  .filter((v: string) => /^(steam|custom):[0-9a-f]+$/.test(v));

const real = [
  ...fromVectors,
  ...fromLibrary,
  "11111111-2222-4333-8444-555555555555", // saved host id
  "192.168.1.50:9777",
  "[fe80::1]:9777",
  "[::1]:7777",
  "deck.local:47989",
  "steam:570",
  "0a1b2c3d4e5f", // preset and profile ids
  "a".repeat(128),
];
for (const id of real) {
  assert.equal(isSafeLaunchId(id), true, `real id refused: ${id}`);
}

// Every ASCII metacharacter and separator an id never holds.
for (const ch of "; | & ( ) { } < > * ? ~ # ! = $ ` \" ' \\ % , + / \t\n\r\0".split("")) {
  assert.equal(isSafeLaunchId(`a${ch}b`), false, `${JSON.stringify(ch)} mid-id`);
  assert.equal(isSafeLaunchId(`ab${ch}`), false, `${JSON.stringify(ch)} at the end`);
}
for (const id of [
  "",
  " ",
  "steam 570",
  "steam:570;id",
  "steam:570$(id)",
  "--browse",
  "-x",
  "é",
  "a".repeat(129),
  "libretro:snes/Super Metroid (USA)",
]) {
  assert.equal(isSafeLaunchId(id), false, `unsafe id passed: ${JSON.stringify(id)}`);
}

for (const bin of [
  "/usr/bin/punktfunk-client",
  "/home/deck/.local/bin/punktfunk-client",
  "/home/deck/.local/share/punktfunk/bin/punktfunk-gtk",
  "/run/media/mmcblk0p1/apps/c++/client-1.2.3",
  "/opt/punktfunk@1/bin/client",
]) {
  assert.equal(isSafeClientBin(bin), true, `real path refused: ${bin}`);
}
for (const bin of [
  "",
  "/",
  "client",
  "./client",
  "~/client",
  "-/client",
  "/home/deck/my apps/client",
  "/a;id",
  "/a|b",
  "/a&b",
  "/a(b)",
  "/a{b}",
  "/a<b",
  "/a>b",
  "/a*b",
  "/a?b",
  "/a~b",
  "/a#b",
  "/a!b",
  "/a=b",
  "/a$b",
  "/a`b`",
  '/a"b',
  "/a'b",
  "/a\\b",
  "/a\nb",
  `/${"a".repeat(128)}`,
]) {
  assert.equal(isSafeClientBin(bin), false, `unsafe path passed: ${JSON.stringify(bin)}`);
}
console.log("launch id tests passed");

// The options string: PF_REF first, the client path only when given, each extra in a fixed
// order, and the wrapper behind %command% as a quoted argument.
const runner = "/home/deck/homebrew/plugins/punktfunk/bin/punktfunkrun.sh";
const tail = `%command% "${runner}"`;
const host = "11111111-2222-4333-8444-555555555555";
assert.equal(launchOptions(host, runner, "", {}), `PF_REF=${host} ${tail}`, "plain stream");
assert.equal(
  launchOptions(host, runner, "/usr/bin/punktfunk-client", {}),
  `PF_REF=${host} PF_CLIENT_BIN=/usr/bin/punktfunk-client ${tail}`,
  "native client",
);
assert.equal(
  launchOptions(host, runner, "/usr/bin/punktfunk-client", {
    profileId: "0a1b2c3d4e5f",
    gameId: "steam:570",
    presetId: "f5e4d3c2b1a0",
  }),
  `PF_REF=${host} PF_CLIENT_BIN=/usr/bin/punktfunk-client PF_PRESET=f5e4d3c2b1a0 ` +
    `PF_GAME=steam:570 PF_AS=0a1b2c3d4e5f ${tail}`,
  "every extra, in the wrapper's order",
);
assert.equal(
  launchOptions("192.168.1.50:9777", runner, "", {}),
  `PF_REF=192.168.1.50:9777 ${tail}`,
  "an unsaved host by address",
);
for (const bin of ["/home/deck/my apps/client", "/a;id", "relative/client"]) {
  assert.throws(() => launchOptions(host, runner, bin, {}), /client path/, `unsafe path ${bin}`);
}

// validateLaunch refuses an unsafe host ref or id before anything reaches Steam.
validateLaunch(host, { presetId: "f5e4d3c2b1a0", gameId: "steam:570", profileId: "0a1b2c3d4e5f" });
validateLaunch("[fe80::1]:9777", {});
for (const [ref, opts, what] of [
  ["--browse", {}, /host reference/],
  ["host;id", {}, /host reference/],
  [host, { presetId: "a b" }, /preset id/],
  [host, { gameId: "steam:570$(id)" }, /game id/],
  [host, { profileId: "-x" }, /profile id/],
] as const) {
  assert.throws(() => validateLaunch(ref, opts), what, `${ref} ${JSON.stringify(opts)}`);
}
console.log("launch options tests passed");
