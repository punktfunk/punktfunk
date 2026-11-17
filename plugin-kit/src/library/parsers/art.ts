// Where a title's cover art lives: Steam's local caches, its per-account `grid/` overrides, and the
// public CDN.
//
// A plugin emits art VALUES and the host serves them: a `file://` URL for anything on disk (the
// documented local-art contract — the host proxies the bytes), or an absolute CDN URL the client
// fetches itself. `data:` URLs remain legal but are small-logo-only: an inlined cover per title
// grows the reconcile body, which the host caps (`DefaultBodyLimit` in mgmt.rs).
import * as path from "node:path";
import { isFile, listDir } from "./fs.js";

/** The four art slots the library model carries. */
export type ArtKind = "portrait" | "hero" | "logo" | "header";

export const ART_KINDS: readonly ArtKind[] = [
	"portrait",
	"hero",
	"logo",
	"header",
];

/** A `file://` URL for a local path — the shape the host's art proxy understands. */
export const fileUrl = (p: string): string => {
	// Percent-encode, but keep the separators: the host converts this back to a path and expects the
	// structure intact. Windows drive paths become `file:///C:/…`.
	const abs = path.resolve(p);
	const posix = abs.replace(/\\/g, "/");
	const encoded = posix
		.split("/")
		.map((seg) => encodeURIComponent(seg))
		.join("/");
	return posix.startsWith("/") ? `file://${encoded}` : `file:///${encoded}`;
};

/**
 * The legacy flat CDN URL for a Steam appid's art kind. Correct for the many titles Valve hasn't
 * re-hashed; newer ones serve from an unpredictable per-asset-hash path, where this 404s and the
 * client falls through to its next candidate. That degradation is intentional.
 */
export const steamCdnUrl = (
	appid: number,
	kind: ArtKind,
): string | undefined => {
	// A non-Steam shortcut's appid has the high bit set and is never a real store appid — the CDN
	// would only 404, so don't emit a URL that is guaranteed to fail.
	if ((appid & 0x8000_0000) !== 0) return undefined;
	const file =
		kind === "portrait"
			? "library_600x900.jpg"
			: kind === "hero"
				? "library_hero.jpg"
				: kind === "logo"
					? "logo.png"
					: "header.jpg";
	return `https://cdn.cloudflare.steamstatic.com/steam/apps/${appid}/${file}`;
};

/**
 * Filenames Steam's local `librarycache` uses per kind, in preference order (2x is sharper).
 *
 * The cover and the header have several spellings each, because Steam renamed these assets and one
 * cache holds both eras side by side. The renamed files are the same assets, byte-for-byte the same
 * shapes (cover 300×450, header 460×215), so which name wins is cosmetic — but knowing only one
 * name loses the art outright.
 *
 * Missing the cover is the one that shows: the fallback is then the flat CDN URL, which 404s for
 * anything Valve has re-hashed, so the client walks on to the header and draws a BANNER in a 2:3
 * poster slot.
 */
const localFilenames = (kind: ArtKind): string[] =>
	kind === "portrait"
		? ["library_600x900_2x.jpg", "library_600x900.jpg", "library_capsule.jpg"]
		: kind === "hero"
			? ["library_hero.jpg"]
			: kind === "logo"
				? ["logo.png"]
				: // Steam's local cache holds the header as both `library_header.jpg` and the CDN's
					// `header.jpg`, so both belong here.
					["library_header.jpg", "header.jpg"];

/**
 * This kind's file under one Steam root's `appcache/librarycache/`, or `undefined`.
 *
 * Three layouts, all of them live in the same cache at the same time, so all three have to be
 * checked or a title's cover is simply not found:
 *
 *   1. `<appid>/<hash>/<name>` — per-asset-version hash dir. Steam reuses one hash dir per version,
 *      so there is normally exactly one candidate. Checked first: where a title has been re-fetched
 *      into this layout, this is the copy Steam itself is displaying.
 *   2. `<appid>/<name>` — straight in the appid dir. A hash-dir-only walk misses every title kept
 *      here.
 *   3. `<appid>_<name>` flat in `librarycache/` — the oldest layout.
 */
export const findLocalArtFile = (
	root: string,
	appid: number,
	kind: ArtKind,
): string | undefined => {
	const base = path.join(root, "appcache", "librarycache", String(appid));
	for (const hash of listDir(base)) {
		for (const name of localFilenames(kind)) {
			const p = path.join(base, hash, name);
			if (isFile(p)) return p;
		}
	}
	// Layout 2: no hash dir, the asset sits directly in the appid dir.
	for (const name of localFilenames(kind)) {
		const p = path.join(base, name);
		if (isFile(p)) return p;
	}
	// Older Steam wrote the files directly under `librarycache/` with the appid in the name.
	for (const name of localFilenames(kind)) {
		const flat = path.join(
			root,
			"appcache",
			"librarycache",
			`${appid}_${name}`,
		);
		if (isFile(flat)) return flat;
	}
	return undefined;
};

/**
 * The `grid/` basenames Steam names each art kind under for an appid: portrait `<A>p`, hero
 * `<A>_hero`, logo `<A>_logo`, wide capsule `<A>` — each as `.png` then `.jpg`.
 *
 * These overrides are the **only** art a non-Steam shortcut ever has.
 */
export const gridFilenames = (appid: number, kind: ArtKind): string[] => {
	const base =
		kind === "portrait"
			? `${appid}p`
			: kind === "hero"
				? `${appid}_hero`
				: kind === "logo"
					? `${appid}_logo`
					: `${appid}`;
	return [`${base}.png`, `${base}.jpg`];
};

/** This kind's user override under a `userdata/<id>/config/grid/` dir, or `undefined`. */
export const findGridArtFile = (
	configDir: string,
	appid: number,
	kind: ArtKind,
): string | undefined => {
	const grid = path.join(configDir, "grid");
	for (const name of gridFilenames(appid, kind)) {
		const p = path.join(grid, name);
		if (isFile(p)) return p;
	}
	return undefined;
};
