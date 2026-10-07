/// <reference types="vite/client" />

import type { QueryClient } from "@tanstack/react-query";
import {
	createRootRouteWithContext,
	HeadContent,
	Outlet,
	Scripts,
	useRouterState,
} from "@tanstack/react-router";
import "@fontsource-variable/geist";
import { Toaster } from "@unom/ui/toast";
import { MotionConfig } from "motion/react";
import { type CSSProperties, useEffect } from "react";
import { useUiConfig } from "@/api/uiConfig";
import { AppShell } from "@/components/app-shell";
import { DialogsProvider } from "@/components/dialogs";
import { currentAppearance } from "@/lib/appearanceRequest";
import { useDensity } from "@/lib/density";
import { adoptStoredLocale, useLocale } from "@/lib/i18n";
import appCss from "@/styles.css?url";

export interface RouterContext {
	queryClient: QueryClient;
}

export const Route = createRootRouteWithContext<RouterContext>()({
	head: () => ({
		meta: [
			{ charSet: "utf-8" },
			{ name: "viewport", content: "width=device-width, initial-scale=1" },
			{ name: "color-scheme", content: "dark light" },
			{ name: "theme-color", content: "#6c5bf3" },
			{ name: "apple-mobile-web-app-capable", content: "yes" },
			{ name: "apple-mobile-web-app-title", content: "Punktfunk" },
			{ title: "Punktfunk" },
		],
		links: [
			{ rel: "stylesheet", href: appCss },
			{ rel: "icon", type: "image/svg+xml", href: "/favicon.svg" },
			// Installable on a phone — this console is used from a couch as often as from a desk,
			// and a home-screen launcher beats retyping a LAN IP. Standalone display, no service
			// worker: an offline shell for a console whose every screen is live host state would
			// only ever show stale numbers convincingly.
			{ rel: "manifest", href: "/manifest.webmanifest" },
		],
	}),
	component: RootComponent,
});

function RootComponent() {
	// Adopt the persisted/browser locale AFTER hydration — the initial render stays at the base
	// locale to match SSR (see lib/i18n.ts), so this is the single, mismatch-free locale switch.
	useEffect(() => {
		adoptStoredLocale();
	}, []);
	// `lang` must track the locale the page is actually rendered in — it is what tells a screen
	// reader which pronunciation to use, and it was pinned to "en" while the app switched to German
	// underneath it. `adoptStoredLocale` also sets it on the live document; this keeps SSR honest.
	const locale = useLocale();
	// The login screen renders bare (no sidebar); everything else gets the app shell.
	const isLogin = useRouterState({
		select: (s) => s.location.pathname === "/login",
	});
	// Follow the desktop's own theme, and let the operator override it (§7).
	//
	// Two attributes, because two kinds of source exist: `data-accent` re-tints the brand from
	// one colour, `data-omarchy` additionally repaints every surface out of the theme's own
	// background/foreground pair. A desktop that publishes only an accent — GNOME over the XDG
	// portal, Windows' DWM — switches on the first and leaves the console's surfaces alone.
	//
	// The expansion lives in CSS rather than here on purpose: `color-mix()` does it natively, in
	// one place, for both modes at once — and it is the only way `.dark`'s own values get
	// overridden without this component knowing which of them each mode uses.
	const { data: uiConfig } = useUiConfig();
	const theme = uiConfig?.theme ?? null;
	// The operator's own pick outranks the desktop. Read isomorphically, so the server render
	// already carries it and there is no flash to correct after hydration.
	const appearance = currentAppearance();
	const accent =
		appearance.accent !== "system"
			? appearance.accent
			: (theme?.accent ?? null);
	const mode =
		appearance.mode !== "system" ? appearance.mode : (theme?.mode ?? "dark");
	// Surfaces need the full pair; a theme carrying only an accent must not switch them on.
	//
	// And they are dropped outright when the operator overrides the MODE: Omarchy publishes one
	// palette, for its own light or dark theme, so painting a dark theme's surfaces under a
	// forced light mode left the page dark while the semantic colours went light. Following the
	// desktop is all-or-nothing; the accent still carries over.
	const modeOverridden =
		appearance.mode !== "system" && appearance.mode !== theme?.mode;
	// An accent the operator CHOSE outranks the desktop's whole palette, not just its own
	// accent: picking a colour and watching the page stay the desktop's is the thing this
	// setting is for. A followed accent leaves the desktop's surfaces alone.
	const accentChosen = appearance.accent !== "system";
	const surfaces =
		!modeOverridden && theme?.background && theme.foreground ? theme : null;
	const density = useDensity();
	const vars: Record<string, string> = {};
	if (accent) vars["--pf-accent"] = accent;
	if (surfaces) {
		vars["--pf-bg"] = surfaces.background as string;
		vars["--pf-fg"] = surfaces.foreground as string;
	}
	return (
		<html
			lang={locale}
			className={mode === "light" ? undefined : "dark"}
			data-accent={accent ? (accentChosen ? "custom" : "") : undefined}
			data-omarchy={surfaces ? "" : undefined}
			data-density={density}
			style={Object.keys(vars).length > 0 ? (vars as CSSProperties) : undefined}
		>
			<head>
				<HeadContent />
			</head>
			<body className="min-h-screen">
				{/* Motion defaults to `reducedMotion: "never"`, so every card, nav item and button
				    animated at full strength even for someone whose OS asks for less. "user" honours
				    the OS setting. */}
				<MotionConfig reducedMotion="user">
					{/* The console's own confirm/prompt, in place of the browser's grey boxes. Mounted
					    at the root because the navigation guard on the Displays page asks for one
					    while LEAVING that page — see components/dialogs.tsx. */}
					<DialogsProvider>
						{isLogin ? (
							<Outlet />
						) : (
							<AppShell>
								<Outlet />
							</AppShell>
						)}
					</DialogsProvider>
				</MotionConfig>
				{/* Sonner toaster (lazy client-side) — success feedback for auto-saved settings. */}
				<Toaster />
				<Scripts />
			</body>
		</html>
	);
}
