import { createFileRoute } from "@tanstack/react-router";
import { SectionControllers } from "@/sections/Diagnostics/Controllers";

export const Route = createFileRoute("/diagnostics/controllers")({
	component: SectionControllers,
});
