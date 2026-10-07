// Import the console's REAL stylesheet directly (rememed-style) — the @theme
// blocks process because this is the literal entry Storybook's Vite pipeline sees.
import "../src/styles.css";
// The console loads its brand typeface separately (in __root.tsx); do the same
// here or every story falls back to system-ui and looks off.
import "@fontsource-variable/geist";
import { definePreview } from "@storybook/react-vite";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { defaultMaterialTheme, MaterialProvider } from "@unom/ui/material";
import Section from "@unom/ui/section";
import { useEffect } from "react";
import { DialogsProvider } from "../src/components/dialogs";

// React Query is present so any query-backed component mounts without a real
// host. Stories should feed mock data rather than fetch — retries are off so a
// stray request fails fast instead of hanging the canvas.
const queryClient = new QueryClient({
	defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
});

export default definePreview({
	addons: [],
	// The live console pins dark; default the canvas to dark too, with a toolbar
	// switch to preview the light theme while designing.
	initialGlobals: { theme: "dark" },
	globalTypes: {
		theme: {
			description: "Light/dark color scheme",
			toolbar: {
				title: "Theme",
				icon: "circlehollow",
				items: [
					{ value: "dark", icon: "moon", title: "Dark" },
					{ value: "light", icon: "sun", title: "Light" },
				],
				dynamicTitle: true,
			},
		},
		// The phone A/B (D-F): a page inside the shell's phone gutter, in either shape.
		// `&globals=density:flush` at 390 px is the screenshot. Deleted with the pick.
		density: {
			description: "Phone density A/B",
			toolbar: {
				title: "Density",
				items: [
					{ value: "", title: "Off" },
					{ value: "flush", title: "Flush lists" },
					{ value: "cards", title: "Cards" },
				],
				dynamicTitle: true,
			},
		},
	},
	decorators: [
		(Story, context) => {
			const dark = (context.globals.theme as string) !== "light";
			const density = (context.globals.density as string) || undefined;
			// `layout: 'fullscreen'` stories (e.g. the AppShell) own their own padding;
			// everything else gets a comfortable inset.
			const fullscreen = context.parameters.layout === "fullscreen";
			// Mirror `.dark` onto <html> so the body's token-driven background AND any
			// portal-mounted content (radix dialogs, popovers) pick up the right
			// palette — the console keys its whole token set off `html.dark`.
			useEffect(() => {
				document.documentElement.classList.toggle("dark", dark);
			}, [dark]);
			useEffect(() => {
				if (density) document.documentElement.dataset.density = density;
				else delete document.documentElement.dataset.density;
			}, [density]);
			return (
				<QueryClientProvider client={queryClient}>
					<MaterialProvider theme={defaultMaterialTheme}>
						{/* The console's confirm/prompt, same as __root mounts. Any section that can
						    delete or discard something calls `useDialogs()`, which THROWS without
						    this — so it belongs beside the other app-level providers rather than in
						    each story's decorators. */}
						<DialogsProvider>
							<div className={dark ? "dark" : ""}>
								<Section maxWidth={false}>
									{density ? (
										// -m-4 undoes the padded layout's inset: the gutter is the shell's.
										<main className="-m-4 min-h-screen bg-background px-4 py-6 text-foreground">
											<Story />
										</main>
									) : (
										<div
											className={`min-h-screen bg-background text-foreground ${fullscreen ? "" : "p-6"}`}
										>
											<Story />
										</div>
									)}
								</Section>
							</div>
						</DialogsProvider>
					</MaterialProvider>
				</QueryClientProvider>
			);
		},
	],
	parameters: {
		controls: { matchers: { color: /(background|color)$/i, date: /Date$/ } },
		layout: "padded",
	},
});
