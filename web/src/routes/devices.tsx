import { createFileRoute } from "@tanstack/react-router";
import { SectionDevices } from "@/sections/Devices";

export const Route = createFileRoute("/devices")({ component: SectionDevices });
