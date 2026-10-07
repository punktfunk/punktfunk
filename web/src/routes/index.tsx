import { createFileRoute } from "@tanstack/react-router";
import { SectionHome } from "@/sections/Home";

export const Route = createFileRoute("/")({ component: SectionHome });
