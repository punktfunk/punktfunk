import { useRouterState } from "@tanstack/react-router";
import { useEffect, useState } from "react";

/**
 * The phone layout's A/B (design/web-console-structure-2026-10.md §6, D-F): `?density=flush` or
 * `?density=cards` picks a shape until the next page load, and a pill flips it in place. Nothing
 * is stored. Deleted with the maintainer's pick, along with the `flush` rule in styles.css.
 */
export type Density = "flush" | "cards";

const parse = (v: unknown): Density | undefined =>
	v === "flush" || v === "cards" ? v : undefined;

/** The shape a URL asked for, or none; a later navigation without the parameter keeps it. */
export function useDensity(): [Density | undefined, (d: Density) => void] {
	const fromUrl = useRouterState({
		select: (s) =>
			parse((s.location.search as Record<string, unknown>).density),
	});
	const [kept, setKept] = useState(fromUrl);
	useEffect(() => {
		if (fromUrl) setKept(fromUrl);
	}, [fromUrl]);
	return [kept ?? fromUrl, setKept];
}

/** The A/B's switch: names the shape on screen, a tap shows the other. Phone width only. */
export const DensitySwitch = ({
	density,
	onFlip,
}: {
	density: Density;
	onFlip: (d: Density) => void;
}) => (
	<button
		type="button"
		className="fixed top-2 right-3 z-50 rounded-full bg-primary px-3 py-1.5 text-xs font-semibold text-primary-foreground shadow-lg sm:hidden"
		onClick={() => onFlip(density === "flush" ? "cards" : "flush")}
	>
		{density} ⇄
	</button>
);
