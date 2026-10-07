import { createFileRoute } from "@tanstack/react-router";
import { SectionLogs } from "@/sections/Diagnostics/Logs";

export const Route = createFileRoute("/diagnostics/logs")({
	component: SectionLogs,
});
