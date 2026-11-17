import { QueryClient } from "@tanstack/react-query";
import { createRouter as createTanStackRouter } from "@tanstack/react-router";
import { reloadOnStaleChunk } from "@unom/ui/preload-reload";
import { ApiError } from "./api/fetcher";
import { routeTree } from "./routeTree.gen";

function createQueryClient() {
	return new QueryClient({
		defaultOptions: {
			queries: {
				staleTime: 2_000,
				// Don't hammer the host on auth/validation errors; do retry transient 5xx once.
				retry: (failureCount, error) => {
					if (
						error instanceof ApiError &&
						error.status >= 400 &&
						error.status < 500
					)
						return false;
					return failureCount < 1;
				},
			},
		},
	});
}

/**
 * The browser's ONE QueryClient.
 *
 * `getRouter()` can run more than once per page load (hydration discards and rebuilds the tree),
 * and a fresh client each time means a fresh, empty cache that nothing else holds a reference to.
 * The event stream invalidates the client it was last handed, so that must be the one the live
 * pages read. One client per browser session keeps the cache across a router rebuild.
 *
 * Deliberately browser-only: on the SERVER every request must get its OWN client, or one visitor's
 * data would be served from another's cache.
 */
let browserQueryClient: QueryClient | undefined;

// Same rebuild, same reason: `reloadOnStaleChunk` registers once, so its callback reads
// whichever router is live at event time instead of the one discarded by hydration.
let liveRouter: { latestLocation: { href: string } } | undefined;

export function getRouter() {
	let queryClient: QueryClient;
	if (typeof window === "undefined") {
		queryClient = createQueryClient();
	} else {
		if (!browserQueryClient) browserQueryClient = createQueryClient();
		queryClient = browserQueryClient;
	}

	const router = createTanStackRouter({
		routeTree,
		context: { queryClient },
		defaultPreload: "intent",
		scrollRestoration: true,
		Wrap: ({ children }) => (
			<QueryProvider client={queryClient}>{children}</QueryProvider>
		),
	});

	liveRouter = router;

	// A deploy replaces every hashed chunk, so a console left open across one asks for files
	// the new server has never heard of and the next navigation dies — with `defaultPreload:
	// "intent"`, a hover is enough to trip it. Handing back where the router was heading means
	// the click that tripped it still lands on the page the operator asked for; during a
	// hover-preload that is the current URL, which degrades to a plain reload.
	reloadOnStaleChunk(10_000, () => liveRouter?.latestLocation.href);

	return router;
}

// Local import kept below the function so the module reads top-down.
import { QueryClientProvider } from "@tanstack/react-query";

function QueryProvider({
	client,
	children,
}: {
	client: QueryClient;
	children: React.ReactNode;
}) {
	return <QueryClientProvider client={client}>{children}</QueryClientProvider>;
}

declare module "@tanstack/react-router" {
	interface Register {
		router: ReturnType<typeof getRouter>;
	}
}
