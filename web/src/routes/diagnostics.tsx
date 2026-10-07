import { createFileRoute } from "@tanstack/react-router";
import { SectionDiagnostics } from "@/sections/Diagnostics";

export const Route = createFileRoute("/diagnostics")({
	component: SectionDiagnostics,
});
