import { createFileRoute, redirect } from "@tanstack/react-router";

// The page moved to `/diagnostics/performance`. Kept for one release so bookmarks land; then delete.
export const Route = createFileRoute("/stats")({
	beforeLoad: () => {
		throw redirect({ to: "/diagnostics/performance", replace: true });
	},
});
