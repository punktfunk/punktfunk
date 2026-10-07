import { useRouterState } from "@tanstack/react-router";
import { useEffect, useState } from "react";

/**
 * The phone layout's A/B (design/web-console-structure-2026-10.md §6, D-F): `?density=flush` or
 * `?density=cards` picks a shape until the next page load. Nothing is stored. Deleted with the
 * maintainer's pick, along with the `flush` rule in styles.css.
 */
export type Density = "flush" | "cards";

const parse = (v: unknown): Density | undefined =>
	v === "flush" || v === "cards" ? v : undefined;

/** The shape this page load renders; a later navigation without the parameter keeps it. */
export function useDensity(): Density {
	const fromUrl = useRouterState({
		select: (s) =>
			parse((s.location.search as Record<string, unknown>).density),
	});
	const [kept, setKept] = useState(fromUrl);
	useEffect(() => {
		if (fromUrl) setKept(fromUrl);
	}, [fromUrl]);
	return fromUrl ?? kept ?? "cards";
}
