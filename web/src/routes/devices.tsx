import { createFileRoute } from "@tanstack/react-router";
import { SectionDevices } from "@/sections/Devices";

/** `pair` opens the Pair sheet: Home's idle card and the command palette link here with it. */
export const Route = createFileRoute("/devices")({
	validateSearch: (s: Record<string, unknown>): { pair?: true } =>
		s.pair === true ? { pair: true } : {},
	component: SectionDevices,
});
