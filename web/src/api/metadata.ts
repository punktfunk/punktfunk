// An Art & Metadata source's own surface (`/__metadata/*` on the plugin), through the console's
// `/api/plugin-metadata/<id>/<route>` BFF. Hand-written: the plugin serves it, not the host API.
import { useQuery } from "@tanstack/react-query";
import { pluginSurfaceOrThrow } from "./pluginSurface";

export interface SourceStatus {
	ready: boolean;
	/** Why the source is not filling anything, in the plugin's words. */
	reason?: string;
	wanted: number;
	found: number;
	lastRun?: number;
	searchable: boolean;
}

export interface SourceMatch {
	match: { key: string; label: string } | null;
	pinned: boolean;
}

export interface SourceCandidate {
	key: string;
	label: string;
	thumb: string | null;
}

export interface SourceImage {
	url: string;
	thumb?: string;
	label?: string;
	width?: number;
	height?: number;
}

const url = (
	id: string,
	route: string,
	params: Record<string, string> = {},
) => {
	const qs = new URLSearchParams(params).toString();
	return `/api/plugin-metadata/${id}/${route}${qs ? `?${qs}` : ""}`;
};

const call = <T>(
	id: string,
	route: string,
	params: Record<string, string>,
	send?: { method: "PUT" | "POST"; body: unknown },
): Promise<T> => pluginSurfaceOrThrow<T>(url(id, route, params), send);

export const sourceKey = (id: string, ...rest: string[]) => [
	"metadata-source",
	id,
	...rest,
];

/** A source's status line. Polled slowly: a round can take minutes. */
export function useSourceStatus(id: string, enabled = true) {
	return useQuery({
		queryKey: sourceKey(id, "status"),
		queryFn: () => call<SourceStatus>(id, "status", {}),
		enabled,
		refetchInterval: 30_000,
		retry: false,
	});
}

export function useSourceMatch(id: string, entry: string) {
	return useQuery({
		queryKey: sourceKey(id, "match", entry),
		queryFn: () => call<SourceMatch>(id, "match", { entry }),
		retry: false,
	});
}

export function useSourceImages(id: string, entry: string, kind: string) {
	return useQuery({
		queryKey: sourceKey(id, "images", entry, kind),
		queryFn: () =>
			call<{ images: SourceImage[] }>(id, "images", { entry, kind }).then(
				(r) => r.images,
			),
		retry: false,
	});
}

export const searchSource = (id: string, entry: string, term: string) =>
	call<{ candidates: SourceCandidate[] }>(
		id,
		"search",
		{},
		{ method: "POST", body: { entry, term } },
	).then((r) => r.candidates);

/** Pin the game this entry is in the source's catalog; `null` goes back to the source's own match. */
export const setSourceMatch = (id: string, entry: string, key: string | null) =>
	call<SourceMatch>(id, "match", {}, { method: "PUT", body: { entry, key } });

/**
 * An `http(s)` URL the host will store as a pick: at most 2048 UTF-8 bytes, no whitespace or
 * control characters. clients/shared/library-id-vectors.json pins it.
 */
export const isHttpUrl = (v: string): boolean =>
	/^https?:\/\/./.test(v) &&
	new TextEncoder().encode(v).length <= 2048 &&
	![...v].some((c) => {
		const n = c.charCodeAt(0);
		return n < 0x20 || (n >= 0x7f && n <= 0x9f) || /\s/.test(c);
	});
