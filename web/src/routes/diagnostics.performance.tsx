import { createFileRoute } from "@tanstack/react-router";
import { SectionStats } from "@/sections/Diagnostics/Performance";

export const Route = createFileRoute("/diagnostics/performance")({
	component: SectionStats,
});
