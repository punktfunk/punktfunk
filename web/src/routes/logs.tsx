import { createFileRoute, redirect } from "@tanstack/react-router";

// The page moved to `/diagnostics/logs`. Kept for one release so bookmarks land; then delete.
export const Route = createFileRoute("/logs")({
	beforeLoad: () => {
		throw redirect({ to: "/diagnostics/logs", replace: true });
	},
});
