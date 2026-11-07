// Build src/data.json — the payload the Worker serves — from published winget manifests.
//
// Source of truth is the Gitea RELEASES, not local files: windows-host.yml attaches the manifests
// to each stable tag, so walking releases yields every version the source should offer. That
// matters because winget resolves `--version` and `upgrade` against the version LIST; a source that
// only knows the newest release can neither pin an older one nor show an upgrade path from it.
//
// Deriving from releases also means this script holds no state. Re-running it reproduces the same
// data.json, so a lost deployment is one command away and there is no accumulated file to drift.
//
//   node build-data.mjs                       # from Gitea releases (what CI runs)
//   node build-data.mjs --local ../           # from a local manifest dir (dev, single version)
//   node build-data.mjs --out src/data.json
//
// The `yaml` dep is a BUILD-time dependency only — the Worker ships data.json, never a parser.
import { readFileSync, writeFileSync, readdirSync, mkdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parse as parseYaml } from "yaml";

const HERE = dirname(fileURLToPath(import.meta.url));
const API = process.env.PF_GITEA_API ?? "https://git.unom.io/api/v1";
const REPO = process.env.PF_GITEA_REPO ?? "unom/punktfunk";
const PACKAGE_ID = "unom.PunktfunkHost";
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

const DEFAULT_LOCALE = `${PACKAGE_ID}.locale.en-US.yaml`;
/** Every other locale manifest's file name; its fields replace the default's in that language. */
const isExtraLocale = (name) =>
  name.startsWith(`${PACKAGE_ID}.locale.`) && name.endsWith(".yaml") && name !== DEFAULT_LOCALE;

/** One version's manifests -> the shape the Worker serves. */
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

async function versionsFromReleases() {
  const releases = await fetchJson(`${API}/repos/${REPO}/releases?limit=100`);
  const out = [];
  for (const rel of releases) {
    if (rel.draft) continue;
    const assets = rel.assets ?? [];
    const find = (suffix) =>
      assets.find((a) => a.name === `${PACKAGE_ID}${suffix}`)?.browser_download_url;
    const installerUrl = find(".installer.yaml");
    const localeUrl = find(".locale.en-US.yaml");
    // Releases from before winget support shipped have no manifests — skip, don't fail.
    if (!installerUrl || !localeUrl) continue;
    const extraUrls = assets
      .filter((a) => isExtraLocale(a.name))
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

function versionsFromLocal(dir) {
  const d = resolve(HERE, dir);
  const names = readdirSync(d);
  const read = (suffix) => {
    const n = names.find((x) => x === `${PACKAGE_ID}${suffix}`);
    if (!n) throw new Error(`missing ${PACKAGE_ID}${suffix} in ${d}`);
    return parseYaml(readFileSync(join(d, n), "utf8"));
  };
  const installer = read(".installer.yaml");
  const locale = read(".locale.en-US.yaml");
  const extra = names.filter(isExtraLocale).sort().map((n) => parseYaml(readFileSync(join(d, n), "utf8")));
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
const collected = localDir ? versionsFromLocal(localDir) : await versionsFromReleases();
if (collected.length === 0) {
  throw new Error("no winget manifests found - refusing to write an empty source");
}

const versions = collected.map((c) => c.entry).sort(compareVersionsDesc);
// Package-level search metadata comes from the NEWEST release's locale manifest, so a rename or a
// retagged description takes effect without rewriting history.
const newestLocale = collected.sort((a, b) => compareVersionsDesc(a.entry, b.entry))[0].locale;

const data = {
  sourceIdentifier: SOURCE_IDENTIFIER,
  serverSupportedVersions: SERVER_SUPPORTED_VERSIONS,
  packages: [
    {
      PackageIdentifier: PACKAGE_ID,
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
      Manifest: { PackageIdentifier: PACKAGE_ID, Versions: versions },
    },
  ],
};

mkdirSync(dirname(outPath), { recursive: true });
writeFileSync(outPath, `${JSON.stringify(data, null, 2)}\n`);
console.log(`wrote ${outPath} (${versions.length} version(s): ${versions.map((v) => v.PackageVersion).join(", ")})`);
