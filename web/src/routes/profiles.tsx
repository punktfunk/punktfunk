import { createFileRoute } from "@tanstack/react-router";
import { SectionProfiles } from "@/sections/Profiles";

export const Route = createFileRoute("/profiles")({
	component: SectionProfiles,
});
