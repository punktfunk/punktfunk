import { createFileRoute, redirect } from "@tanstack/react-router";

// The page moved to `/devices`. Kept for one release so bookmarks land; then delete.
export const Route = createFileRoute("/pairing")({
	beforeLoad: () => {
		throw redirect({ to: "/devices", replace: true });
	},
});
