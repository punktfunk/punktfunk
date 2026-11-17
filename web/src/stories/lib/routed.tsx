import {
	createMemoryHistory,
	createRootRoute,
	createRouter,
	RouterProvider,
} from "@tanstack/react-router";
import { type ReactNode, useMemo } from "react";

/**
 * A throwaway router, so a view with a router `<Link>` renders in a story. Memoised on `children`:
 * a router rebuilt on every render remounts everything under it and restarts the entrance
 * animation.
 */
export function Routed({ children }: { children: ReactNode }) {
	const router = useMemo(
		() =>
			createRouter({
				routeTree: createRootRoute({ component: () => <>{children}</> }),
				history: createMemoryHistory({ initialEntries: ["/"] }),
			}),
		[children],
	);
	// biome-ignore lint/suspicious/noExplicitAny: a throwaway router, not the app's typed tree
	return <RouterProvider router={router as any} />;
}
