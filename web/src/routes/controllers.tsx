import { createFileRoute, redirect } from "@tanstack/react-router";

// The page moved to `/diagnostics/controllers`. Kept for one release so bookmarks land; then delete.
export const Route = createFileRoute("/controllers")({
	beforeLoad: () => {
		throw redirect({ to: "/diagnostics/controllers", replace: true });
	},
});
