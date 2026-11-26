import { createFileRoute } from "@tanstack/react-router";
import { SectionHost } from "@/sections/Host";

/** `q` opens the settings searched for it: the command palette's settings land here with it. */
export const Route = createFileRoute("/host")({
	validateSearch: (s: Record<string, unknown>): { q?: string } =>
		typeof s.q === "string" ? { q: s.q } : {},
	component: SectionHost,
});
