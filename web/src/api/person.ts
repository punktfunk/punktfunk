// Whose console this is, from the server that holds the person's session.
import { useQuery } from "@tanstack/react-query";
import type { PersonMe } from "@/lib/person";
import { ApiError } from "./fetcher";
import { useGetPluginRuntime } from "./gen/store/store";

export const PERSON_ME_KEY = ["person", "me"] as const;

/** Polled: each read asks the person's seat host, so a session that ended there shows here. */
export const usePersonMe = (enabled = true) =>
	useQuery({
		queryKey: PERSON_ME_KEY,
		queryFn: async (): Promise<PersonMe> => {
			const res = await fetch("/_auth/person/me", {
				credentials: "same-origin",
			});
			const body: unknown = await res.json().catch(() => null);
			if (!res.ok) throw new ApiError(res.status, body, res.statusText);
			return body as PersonMe;
		},
		refetchInterval: 15_000,
		retry: false,
		enabled,
	});

/**
 * Whether the person's own plugins have a page: their seat host's runner answers, installed, on a
 * desktop Punktfunk made and on their own account alike. `pending` until it answers.
 */
export function usePersonPlugins(me: PersonMe | null): {
	offered: boolean;
	pending: boolean;
} {
	const ask = me !== null;
	const runtime = useGetPluginRuntime({
		query: { enabled: ask, retry: false, staleTime: 60_000 },
	});
	return {
		offered: ask && runtime.data?.installed === true,
		pending: ask && runtime.isPending,
	};
}

/** Signs the person out of their console; their seat host forgets the session too. */
export async function endPersonSession(): Promise<void> {
	const res = await fetch("/_auth/person/end", { method: "POST" });
	if (!res.ok) throw new ApiError(res.status, null, res.statusText);
}
