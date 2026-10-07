import { createFileRoute, redirect } from "@tanstack/react-router";

// Diagnostics has no page of its own: it opens on its first segment.
export const Route = createFileRoute("/diagnostics/")({
	beforeLoad: () => {
		throw redirect({ to: "/diagnostics/logs", replace: true });
	},
});
