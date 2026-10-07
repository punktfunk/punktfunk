import type { Meta, StoryObj } from "@storybook/react-vite";
import {
	createMemoryHistory,
	createRootRoute,
	createRoute,
	createRouter,
	RouterProvider,
} from "@tanstack/react-router";
import { AppShell } from "@/components/app-shell";

// AppShell is built from TanStack Router <Link>s, so it needs a router context.
// We stand up a throwaway in-memory router whose routes mirror the nav targets
// (so links resolve + the active highlight works) and render the shell from the
// root route. No loaders/data — purely for designing the chrome offline.
function ShellHarness({ initialPath }: { initialPath: string }) {
	const rootRoute = createRootRoute({
		component: () => (
			<AppShell>
				<div className="space-y-3">
					<h1 className="text-2xl font-semibold tracking-tight">Dashboard</h1>
					<p className="text-muted-foreground">
						Placeholder content — swap routes from the sidebar to preview the
						active state.
					</p>
				</div>
			</AppShell>
		),
	});

	const navPaths = [
		"/",
		"/host",
		"/library",
		"/devices",
		"/profiles",
		"/diagnostics/logs",
		"/settings",
	];
	const navRoutes = navPaths.map((path) =>
		createRoute({
			getParentRoute: () => rootRoute,
			path,
			component: () => null,
		}),
	);
	// Splat so any other <Link> target still resolves without throwing.
	const splat = createRoute({
		getParentRoute: () => rootRoute,
		path: "$",
		component: () => null,
	});

	const router = createRouter({
		routeTree: rootRoute.addChildren([...navRoutes, splat]),
		history: createMemoryHistory({ initialEntries: [initialPath] }),
	});

	return <RouterProvider router={router} />;
}

const meta = {
	title: "Shell/AppShell",
	component: AppShell,
	parameters: { layout: "fullscreen" },
	// AppShell requires `children`; the harness supplies the real content, so this
	// placeholder just satisfies the arg type.
	args: { children: null },
} satisfies Meta<typeof AppShell>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Dashboard: Story = {
	render: () => <ShellHarness initialPath="/" />,
};

export const HostActive: Story = {
	render: () => <ShellHarness initialPath="/host" />,
};
