import { createFileRoute } from "@tanstack/react-router";
import { SectionStats } from "@/sections/Diagnostics/Performance";

/** `recording` opens that recording's detail: Home's Recent sessions link here with it. */
export const Route = createFileRoute("/diagnostics/performance")({
	validateSearch: (s: Record<string, unknown>): { recording?: string } =>
		typeof s.recording === "string" ? { recording: s.recording } : {},
	component: SectionStats,
});
