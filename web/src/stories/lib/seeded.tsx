import { type QueryKey, useQueryClient } from "@tanstack/react-query";
import { type ReactNode, useState } from "react";

/** Puts `data` in the query cache before the story mounts, so its components need no host. */
export function Seeded({
	data,
	children,
}: {
	data: [QueryKey, unknown][];
	children: ReactNode;
}) {
	const qc = useQueryClient();
	useState(() => {
		for (const [key, value] of data) {
			qc.setQueryDefaults(key, { staleTime: Infinity });
			qc.setQueryData(key, value);
		}
		return null;
	});
	return <>{children}</>;
}
