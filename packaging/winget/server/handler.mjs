// Punktfunk's winget REST source — the read path only.
//
// The winget client consumes exactly three endpoints. `microsoft/winget-cli-restsource`'s other 23
// (`/packages/**` CRUD and the manifest writes) are that reference implementation's ADMIN API for
// mutating a CosmosDB — they are not part of what a client calls, and a source whose data is
// generated at release time has nothing to mutate. So this serves:
//
//   GET  /information                        server identity + declared capabilities
//   POST /manifestSearch                     search; 204 when nothing matches
//   GET  /packageManifests/{PackageIdentifier}   the full manifest (optional ?Version=); 204
//                                            for a package or version this source lacks
//
// Contract: documentation/WinGet-1.1.0.yaml in that repo. Every response is wrapped in `Data`
// (ResponseObjectSchema). Note the spec's content-type key is literally `application/Json`; the
// client is not picky about the response's own header, so we send standard `application/json`.
//
// Pure request->response logic with NO I/O and no dependencies: the catalogue is passed in, so
// server.mjs owns reloading it and test.mjs can drive this directly without a server or a network.
// `data` is produced by build-data.mjs from the published release manifests — see the README.

const JSON_HEADERS = { "content-type": "application/json; charset=utf-8" };

const json = (body, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: JSON_HEADERS });

// A bare 204 carries no body — this is how the spec says "no results", distinct from an empty array.
const noContent = () => new Response(null, { status: 204 });

const error = (status, message) =>
  json({ ErrorCode: status, ErrorMessage: message }, status);

// Match fields we deliberately do NOT claim to serve.
//
// `NormalizedPackageNameAndPublisher` is the notable one: winget derives it client-side from ARP
// entries using its own normalization (case folding plus stripping legal suffixes, punctuation and
// version-like tails). Claiming support would mean reimplementing that exactly, and a near-miss
// produces silently wrong correlation between an installed host and this package. ProductCode is
// exact and Inno already gives us one, so correlation rides on that instead.
//
// Command / PackageFamilyName are genuinely absent (no declared commands; not an MSIX). Market is
// not modelled.
const UNSUPPORTED_MATCH_FIELDS = [
  "Command",
  "PackageFamilyName",
  "NormalizedPackageNameAndPublisher",
  "Market",
];

/** Compare one candidate string against a SearchRequestMatch. */
function matches(value, request) {
  if (value == null || request == null) return false;
  const keyword = String(request.KeyWord ?? "");
  const hay = String(value).toLowerCase();
  const needle = keyword.toLowerCase();
  switch (request.MatchType) {
    case "Exact":
      return String(value) === keyword;
    case "StartsWith":
      return hay.startsWith(needle);
    // Fuzzy variants are served as substring rather than pretending to a real fuzzy ranker. That
    // over-matches slightly, which is the safe direction: winget re-ranks and filters what we return.
    case "Substring":
    case "Fuzzy":
    case "FuzzySubstring":
      return hay.includes(needle);
    case "Wildcard":
      return true;
    case "CaseInsensitive":
    default:
      return hay === needle;
  }
}

/** The candidate strings a package offers for a given PackageMatchField. */
function fieldValues(pkg, field) {
  switch (field) {
    case "PackageIdentifier":
      return [pkg.PackageIdentifier];
    case "PackageName":
      return [pkg.PackageName];
    case "Moniker":
      return pkg.Moniker ? [pkg.Moniker] : [];
    case "Tag":
      return pkg.Tags ?? [];
    case "ProductCode":
      return pkg.ProductCodes ?? [];
    default:
      return []; // including everything in UNSUPPORTED_MATCH_FIELDS
  }
}

/** A free-text `Query` sweeps the fields a human would expect to type. */
function matchesQuery(pkg, query) {
  if (!query) return true; // no Query block = match everything, narrowed by Inclusions/Filters
  return ["PackageIdentifier", "PackageName", "Moniker", "Tag"].some((f) =>
    fieldValues(pkg, f).some((v) => matches(v, query)),
  );
}

function matchesOne(pkg, entry) {
  // Both Inclusions and Filters use SearchRequestInclusionAndFilterSchema.
  const req = entry?.RequestMatch;
  return fieldValues(pkg, entry?.PackageMatchField).some((v) => matches(v, req));
}

function search(data, body) {
  const { Query, Inclusions, Filters, FetchAllManifests, MaximumResults } = body ?? {};

  let hits = data.packages.filter((pkg) => {
    if (FetchAllManifests) return true;
    if (!matchesQuery(pkg, Query)) return false;
    // Inclusions: any one may match (OR); an empty/absent list imposes nothing.
    if (Array.isArray(Inclusions) && Inclusions.length > 0) {
      if (!Inclusions.some((i) => matchesOne(pkg, i))) return false;
    }
    // Filters narrow (AND) — every filter must hold.
    if (Array.isArray(Filters) && Filters.length > 0) {
      if (!Filters.every((f) => matchesOne(pkg, f))) return false;
    }
    return true;
  });

  if (Number.isInteger(MaximumResults) && MaximumResults > 0) {
    hits = hits.slice(0, MaximumResults);
  }
  return hits;
}

async function handle(request, data) {
  const url = new URL(request.url);
  // Tolerate a mount under a sub-path (e.g. /winget/information) so the same handler can sit behind
  // a reverse proxy that does not strip a prefix.
  const path = url.pathname.replace(/\/+$/, "");
  const tail = (name) => path === `/${name}` || path.endsWith(`/${name}`);

  if (tail("information") && request.method === "GET") {
    return json({
      Data: {
        SourceIdentifier: data.sourceIdentifier,
        ServerSupportedVersions: data.serverSupportedVersions,
        UnsupportedPackageMatchFields: UNSUPPORTED_MATCH_FIELDS,
        RequiredPackageMatchFields: [],
        UnsupportedQueryParameters: [],
        RequiredQueryParameters: [],
      },
    });
  }

  if (tail("manifestSearch")) {
    if (request.method !== "POST") return error(405, "manifestSearch requires POST");
    let body;
    try {
      body = await request.json();
    } catch {
      return error(400, "malformed JSON body");
    }
    const hits = search(data, body);
    if (hits.length === 0) return noContent();
    return json({
      Data: hits.map((pkg) => ({
        PackageIdentifier: pkg.PackageIdentifier,
        PackageName: pkg.PackageName,
        Publisher: pkg.Publisher,
        Versions: pkg.Versions.map((v) => ({
          PackageVersion: v.PackageVersion,
          ProductCodes: v.ProductCodes ?? [],
        })),
      })),
      RequiredPackageMatchFields: [],
      UnsupportedPackageMatchFields: UNSUPPORTED_MATCH_FIELDS,
    });
  }

  const manifestMatch = path.match(/\/packageManifests\/([^/]+)$/);
  if (manifestMatch) {
    if (request.method !== "GET") return error(405, "packageManifests requires GET");
    const id = decodeURIComponent(manifestMatch[1]);
    // PackageIdentifier is case-insensitive per the winget client's own handling.
    const pkg = data.packages.find(
      (p) => p.PackageIdentifier.toLowerCase() === id.toLowerCase(),
    );
    // winget asks every source for an exact id this way and reads a 404 as a broken source, so a
    // package or version this source lacks is 204: "nothing here", and the other sources answer.
    if (!pkg) return noContent();

    const wanted = url.searchParams.get("Version");
    const versions = wanted
      ? pkg.Manifest.Versions.filter((v) => v.PackageVersion === wanted)
      : pkg.Manifest.Versions;
    if (versions.length === 0) return noContent();

    return json({ Data: { PackageIdentifier: pkg.Manifest.PackageIdentifier, Versions: versions } });
  }

  return error(404, `no route for ${request.method} ${url.pathname}`);
}

/** Wraps handle() so no request can escape as an unhandled rejection. */
export async function respond(request, data) {
  try {
    return await handle(request, data);
  } catch (e) {
    return error(500, `unhandled: ${e?.message ?? e}`);
  }
}

export { handle, search, matches };
