// Build data.json — the catalogue server.mjs serves — from published winget manifests.
//
// Source of truth is the Gitea RELEASES, not local files: windows-host.yml and windows-client.yml
// attach the manifests to each stable tag, so walking releases yields every version the source
// should offer, for each package. That matters because winget resolves `--version` and `upgrade`
// against the version LIST; a source that only knows the newest release can neither pin an older
// one nor show an upgrade path from it.
//
// Deriving from releases also means this script holds no state. Re-running it reproduces the same
// data.json, so a lost deployment is one command away and there is no accumulated file to drift.
//
//   node build-data.mjs --out data/data.json  # from Gitea releases (what CI runs)
//   node build-data.mjs --local ../           # from a local manifest dir (dev, single version)
//
// The `yaml` dep is a BUILD-time dependency only — server.mjs reads data.json, never YAML.
import { readFileSync, writeFileSync, readdirSync, mkdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parse as parseYaml } from "yaml";

const HERE = dirname(fileURLToPath(import.meta.url));
const API = process.env.PF_GITEA_API ?? "https://git.unom.io/api/v1";
const REPO = process.env.PF_GITEA_REPO ?? "unom/punktfunk";
// A package none of the releases carries manifests for is left out, so the source can ship before
// a package's first stable tag.
const PACKAGE_IDS = ["unom.PunktfunkHost", "unom.PunktfunkClient"];
const SOURCE_IDENTIFIER = process.env.PF_SOURCE_ID ?? "unom.punktfunk";
// The API contract this source implements. Advertised via /information so a future client can
// negotiate; bump only alongside a real change to the response shapes.
const SERVER_SUPPORTED_VERSIONS = ["1.1.0"];

const args = process.argv.slice(2);
const argVal = (flag) => {
  const i = args.indexOf(flag);
  return i >= 0 ? args[i + 1] : undefined;
};
const outPath = resolve(HERE, argVal("--out") ?? "src/data.json");
const localDir = argVal("--local");

/** Every other locale manifest's file name; its fields replace the default's in that language. */
const isExtraLocale = (id, name) =>
  name.startsWith(`${id}.locale.`) && name.endsWith(".yaml") && name !== `${id}.locale.en-US.yaml`;

/** One version's manifests -> the shape handler.mjs serves. */
function toVersionEntry(installer, locale, extraLocales = [], productCodeFallback) {
  // The REST manifest nests DefaultLocale, Locales + Installers under each version, whereas the
  // YAML splits them across files keyed by PackageVersion. Strip the per-file bookkeeping that does
  // not belong in the merged object.
  const { PackageIdentifier: _i, PackageVersion, ManifestType: _t, ManifestVersion: _v, Installers, ...installerRest } = installer;
  const { PackageIdentifier: _i2, PackageVersion: _pv, ManifestType: _t2, ManifestVersion: _v2, PackageLocale, ...localeRest } = locale;
  const locales = extraLocales.map(
    ({ PackageIdentifier: _i3, PackageVersion: _pv3, ManifestType: _t3, ManifestVersion: _v3, ...rest }) => rest,
  );

  // Installer-level fields are inherited by each entry in Installers unless overridden there. The
  // REST shape has no inheritance, so fold the parent down into every installer.
  const installers = (Installers ?? []).map((inst) => ({ ...installerRest, ...inst }));

  return {
    PackageVersion,
    DefaultLocale: { PackageLocale, ...localeRest },
    ...(locales.length ? { Locales: locales } : {}),
    Installers: installers,
    ProductCodes: [installer.ProductCode ?? productCodeFallback].filter(Boolean),
  };
}

async function fetchJson(url) {
  const res = await fetch(url, { headers: { accept: "application/json" } });
  if (!res.ok) throw new Error(`GET ${url} -> ${res.status} ${res.statusText}`);
  return res.json();
}

async function fetchText(url) {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`GET ${url} -> ${res.status} ${res.statusText}`);
  return res.text();
}

async function versionsFromReleases(releases, id) {
  const out = [];
  for (const rel of releases) {
    if (rel.draft) continue;
    const assets = rel.assets ?? [];
    const find = (suffix) =>
      assets.find((a) => a.name === `${id}${suffix}`)?.browser_download_url;
    const installerUrl = find(".installer.yaml");
    const localeUrl = find(".locale.en-US.yaml");
    // Releases from before winget support shipped have no manifests — skip, don't fail.
    if (!installerUrl || !localeUrl) continue;
    const extraUrls = assets
      .filter((a) => isExtraLocale(id, a.name))
      .sort((a, b) => a.name.localeCompare(b.name))
      .map((a) => a.browser_download_url);
    const [installer, locale, ...extra] = await Promise.all(
      [installerUrl, localeUrl, ...extraUrls].map((u) => fetchText(u).then(parseYaml)),
    );
    out.push({ entry: toVersionEntry(installer, locale, extra), locale, prerelease: rel.prerelease });
    console.log(`  + ${rel.tag_name} (${installer.PackageVersion})`);
  }
  return out;
}

function versionsFromLocal(dir, id) {
  const d = resolve(HERE, dir);
  const names = readdirSync(d);
  const read = (suffix) => {
    const n = names.find((x) => x === `${id}${suffix}`);
    if (!n) throw new Error(`missing ${id}${suffix} in ${d}`);
    return parseYaml(readFileSync(join(d, n), "utf8"));
  };
  const installer = read(".installer.yaml");
  const locale = read(".locale.en-US.yaml");
  const extra = names.filter((n) => isExtraLocale(id, n)).sort().map((n) => parseYaml(readFileSync(join(d, n), "utf8")));
  console.log(`  + local (${installer.PackageVersion})`);
  return [{ entry: toVersionEntry(installer, locale, extra), locale, prerelease: false }];
}

/** Newest first, so winget's "latest" is the head of the list. */
function compareVersionsDesc(a, b) {
  const parts = (v) => v.PackageVersion.split(/[.\-+]/).map((p) => (/^\d+$/.test(p) ? Number(p) : p));
  const [pa, pb] = [parts(a), parts(b)];
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const x = pa[i] ?? 0;
    const y = pb[i] ?? 0;
    if (x === y) continue;
    // A numeric segment outranks a string one (0.20.0 > 0.20.0-rc1).
    if (typeof x === typeof y) return x > y ? -1 : 1;
    return typeof x === "number" ? -1 : 1;
  }
  return 0;
}

console.log(localDir ? `building from local dir ${localDir}` : `building from ${API}/repos/${REPO} releases`);
const releases = localDir ? [] : await fetchJson(`${API}/repos/${REPO}/releases?limit=100`);

const packages = [];
for (const id of PACKAGE_IDS) {
  console.log(id);
  const collected = localDir ? versionsFromLocal(localDir, id) : await versionsFromReleases(releases, id);
  if (collected.length === 0) {
    console.log("  no manifests on any release - left out");
    continue;
  }
  const versions = collected.map((c) => c.entry).sort(compareVersionsDesc);
  // Package-level search metadata comes from the NEWEST release's locale manifest, so a rename or a
  // retagged description takes effect without rewriting history.
  const newestLocale = collected.sort((a, b) => compareVersionsDesc(a.entry, b.entry))[0].locale;
  packages.push({
    PackageIdentifier: id,
    PackageName: newestLocale.PackageName,
    Publisher: newestLocale.Publisher,
    Moniker: newestLocale.Moniker ?? null,
    Tags: newestLocale.Tags ?? [],
    // Union across versions: an installed 0.19.2 must still correlate after 0.20.0 ships.
    ProductCodes: [...new Set(versions.flatMap((v) => v.ProductCodes))],
    Versions: versions.map((v) => ({
      PackageVersion: v.PackageVersion,
      ProductCodes: v.ProductCodes,
    })),
    Manifest: { PackageIdentifier: id, Versions: versions },
  });
}
if (packages.length === 0) {
  throw new Error("no winget manifests found - refusing to write an empty source");
}

const data = {
  sourceIdentifier: SOURCE_IDENTIFIER,
  serverSupportedVersions: SERVER_SUPPORTED_VERSIONS,
  packages,
};

mkdirSync(dirname(outPath), { recursive: true });
writeFileSync(outPath, `${JSON.stringify(data, null, 2)}\n`);
console.log(`wrote ${outPath} (${packages.map((p) => `${p.PackageIdentifier}: ${p.Versions.map((v) => v.PackageVersion).join(", ")}`).join("; ")})`);
