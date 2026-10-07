import { createFileRoute, redirect } from "@tanstack/react-router";

// The ring moved onto Home, where Show all expands it. Kept for one release; then delete.
export const Route = createFileRoute("/activity")({
	beforeLoad: () => {
		throw redirect({ to: "/", replace: true });
	},
});
