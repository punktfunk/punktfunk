import { createFileRoute, redirect } from "@tanstack/react-router";

// The settings moved onto Host. Kept for one release so bookmarks land; then delete.
export const Route = createFileRoute("/host_/settings")({
	beforeLoad: () => {
		throw redirect({ to: "/host", replace: true });
	},
});
