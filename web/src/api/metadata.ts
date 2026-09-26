// An Art & Metadata source's own surface (`/__metadata/*` on the plugin), through the console's
// `/api/plugin-metadata/<id>/<route>` BFF. Hand-written: the plugin serves it, not the host API.
import { useQuery } from "@tanstack/react-query";

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

async function call<T>(
	id: string,
	route: string,
	params: Record<string, string>,
	init?: { method: "PUT" | "POST"; body: unknown },
): Promise<T> {
	const res = await fetch(url(id, route, params), {
		credentials: "same-origin",
		...(init
			? {
					method: init.method,
					headers: { "content-type": "application/json" },
					body: JSON.stringify(init.body),
				}
			: {}),
	});
	const body = (await res.json().catch(() => null)) as
		| (T & { error?: string; issue?: string })
		| null;
	if (!res.ok || body === null) {
		throw new Error(body?.issue ?? body?.error ?? `${res.status}`);
	}
	return body;
}

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

/** An `http(s)` URL the host will store as a pick. */
export const isHttpUrl = (v: string): boolean =>
	/^https?:\/\/\S+$/.test(v) && v.length <= 2048;
