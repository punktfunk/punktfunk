// Drives the Worker's handle() directly — no Workers runtime, no network, no deploy.
//
// This is the gate that catches a broken source BEFORE winget sees it. A source that returns the
// wrong shape does not error loudly: winget reports "no package found" and the failure looks like a
// missing package rather than a malformed response.
//
//   node test.mjs        (run `npm run build:local` first so data/data.json exists)
import { readFileSync } from "node:fs";
import { handle } from "./handler.mjs";

// The catalogue is an argument, not an import — same way server.mjs passes it — so this suite runs
// against a generated data.json with no server, no network and no container.
const DATA = JSON.parse(readFileSync(new URL("./data/data.json", import.meta.url), "utf8"));
const BASE = "https://winget.example/";
let failed = 0;

function check(name, cond, detail = "") {
  if (cond) {
    console.log(`ok   ${name}`);
  } else {
    failed++;
    console.log(`FAIL ${name}${detail ? ` — ${detail}` : ""}`);
  }
}

const get = (p) => handle(new Request(BASE + p, { method: "GET" }), DATA);
const post = (p, body) =>
  handle(new Request(BASE + p, { method: "POST", body: JSON.stringify(body), headers: { "content-type": "application/json" } }), DATA);

// ── /information ────────────────────────────────────────────────────────────────────────────────
{
  const res = await get("information");
  const body = await res.json();
  check("information: 200", res.status === 200, `got ${res.status}`);
  check("information: wrapped in Data", !!body.Data);
  check("information: has SourceIdentifier", typeof body.Data?.SourceIdentifier === "string");
  check("information: advertises a version", Array.isArray(body.Data?.ServerSupportedVersions) && body.Data.ServerSupportedVersions.length > 0);
  check(
    "information: declares NormalizedPackageNameAndPublisher unsupported",
    body.Data?.UnsupportedPackageMatchFields?.includes("NormalizedPackageNameAndPublisher"),
  );
}

// ── /manifestSearch ─────────────────────────────────────────────────────────────────────────────
{
  const res = await post("manifestSearch", { Query: { KeyWord: "punktfunk", MatchType: "Substring" } });
  const body = await res.json();
  check("search substring 'punktfunk': 200", res.status === 200, `got ${res.status}`);
  check("search: Data is a non-empty array", Array.isArray(body.Data) && body.Data.length > 0);
  const hit = body.Data?.[0];
  check("search: PackageIdentifier present", hit?.PackageIdentifier === "unom.PunktfunkHost");
  check("search: PackageName + Publisher present (schema-required)", !!hit?.PackageName && !!hit?.Publisher);
  check("search: Versions non-empty (minItems 1)", Array.isArray(hit?.Versions) && hit.Versions.length > 0);
  check("search: version carries ProductCodes for ARP correlation", Array.isArray(hit?.Versions?.[0]?.ProductCodes) && hit.Versions[0].ProductCodes.length > 0);
}
{
  const res = await post("manifestSearch", { Query: { KeyWord: "definitely-not-a-package", MatchType: "Substring" } });
  check("search miss: 204 No Content, not an empty 200", res.status === 204, `got ${res.status}`);
}
{
  const res = await post("manifestSearch", { FetchAllManifests: true });
  const body = await res.json();
  check("FetchAllManifests returns everything", res.status === 200 && body.Data.length >= 1);
}
{
  const res = await post("manifestSearch", {
    Inclusions: [{ PackageMatchField: "PackageIdentifier", RequestMatch: { KeyWord: "unom.PunktfunkHost", MatchType: "CaseInsensitive" } }],
  });
  check("Inclusions on PackageIdentifier match", res.status === 200);
}
{
  // A filter that cannot hold must exclude the package (Filters are AND).
  const res = await post("manifestSearch", {
    Filters: [{ PackageMatchField: "PackageIdentifier", RequestMatch: { KeyWord: "some.OtherPackage", MatchType: "Exact" } }],
  });
  check("non-matching Filter excludes (AND semantics)", res.status === 204, `got ${res.status}`);
}
{
  // An unsupported field must never accidentally match — we told the client we don't serve it.
  const res = await post("manifestSearch", {
    Filters: [{ PackageMatchField: "NormalizedPackageNameAndPublisher", RequestMatch: { KeyWord: "punktfunkhostunom", MatchType: "Exact" } }],
  });
  check("unsupported match field yields no match", res.status === 204, `got ${res.status}`);
}
{
  const res = await handle(new Request(BASE + "manifestSearch", { method: "GET" }), DATA);
  check("manifestSearch rejects GET", res.status === 405, `got ${res.status}`);
}

// ── /packageManifests/{id} ──────────────────────────────────────────────────────────────────────
{
  const res = await get("packageManifests/unom.PunktfunkHost");
  const body = await res.json();
  check("manifest: 200", res.status === 200, `got ${res.status}`);
  const v = body.Data?.Versions?.[0];
  check("manifest: Versions present", !!v);
  check("manifest: version has DefaultLocale", !!v?.DefaultLocale?.PackageName);
  check("manifest: version has Installers", Array.isArray(v?.Installers) && v.Installers.length > 0);
  const inst = v?.Installers?.[0];
  check("manifest: installer has URL + sha256", !!inst?.InstallerUrl && /^[0-9A-F]{64}$/i.test(inst?.InstallerSha256 ?? ""));
  // `exe` from the M5 swap on; `inno` for every release before it (the catalogue carries both).
  check("manifest: installer-level fields folded into the entry", ["exe", "inno"].includes(inst?.InstallerType) && inst?.Scope === "machine");
  check("manifest: ProductCode preserved for correlation", !!inst?.ProductCode || !!v?.ProductCodes?.length);
  // A Locales entry without PackageLocale, or with per-file keys left in, fails the schema.
  check(
    "manifest: each extra locale names its PackageLocale and nothing per-file",
    (v?.Locales ?? []).every((l) => l.PackageLocale && !l.ManifestType && !l.PackageVersion),
    `got ${JSON.stringify(v?.Locales?.map((l) => l.PackageLocale))}`,
  );
  // A Log switch is only useful if winget SUBSTITUTES the path. <LOGPATH> is the one token it
  // replaces; anything else is passed through verbatim, and `|LOGPATH|` shipped in 0.20.0 — Inno
  // then rejected `|` as a filename and aborted with "Error creating log file", in EVERY install
  // mode, for any caller that requests a log (UniGetUI does by default). Reported from the field.
  const log = inst?.InstallerSwitches?.Log;
  check(
    "manifest: Log switch uses winget's <LOGPATH> token, if present",
    log === undefined || (log.includes("<LOGPATH>") && !/\|[A-Z]+\|/.test(log)),
    `got ${JSON.stringify(log)}`,
  );
}
{
  const res = await get("packageManifests/UNOM.PUNKTFUNKHOST");
  check("manifest: PackageIdentifier is case-insensitive", res.status === 200, `got ${res.status}`);
}
{
  const res = await get("packageManifests/nope.NotHere");
  check("manifest: unknown id -> 404", res.status === 404, `got ${res.status}`);
}
{
  const res = await get("packageManifests/unom.PunktfunkHost?Version=0.0.0-nope");
  check("manifest: unknown ?Version -> 404", res.status === 404, `got ${res.status}`);
}
{
  const res = await get("no/such/route");
  check("unknown route -> 404", res.status === 404, `got ${res.status}`);
}

console.log(failed === 0 ? "\nall checks passed" : `\n${failed} CHECK(S) FAILED`);
process.exit(failed === 0 ? 0 : 1);
