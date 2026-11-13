// GET /_auth/ui-config — the handful of deployment facts the console UI cannot work out for itself.
//
// Two today: where plugin UIs live, and whether the console is reachable beyond this machine.
// They are served from a different ORIGIN than
// the console (2026-08-05 review H-3), so the browser needs the port to build the iframe URL — and
// it must come from the server, because only the server knows whether that listener actually bound.
//
// Public (the `/_auth/` prefix is), which is fine: a port number is discoverable by connecting to
// it, and nothing here is a secret. Deliberately NOT an inference the client makes for itself
// (`location.port + 1` would silently point at whatever else is on that port).
import { defineEventHandler } from "h3";
import { hasTheme, hostTheme } from "../../util/hostTheme";
import { type OmarchyTheme, omarchyTheme } from "../../util/omarchyTheme";
import {
	boundAddress,
	isLoopbackBind,
	pluginOriginPort,
} from "../../util/pluginOrigin";

export interface UiConfig {
	/**
	 * How plugin UIs are reachable:
	 *  - `origin`      — from their own origin on `pluginPort` (the deployed, secure arrangement)
	 *  - `same-origin` — `vite dev` only: one listener, and its own middleware serves `/plugin-ui`
	 *  - `unavailable` — the plugin listener could not bind. Plugin UIs are OFF; the console must
	 *                    not fall back to its own origin, which is the hole this all exists to close.
	 */
	pluginUi: "origin" | "same-origin" | "unavailable";
	pluginPort: number | null;
	/**
	 * The desktop's own colours, so the page matches the box that launched it.
	 *
	 * Omarchy first: it renders our template, so it carries all four values. Otherwise the
	 * host's own read of the desktop — the XDG portal on Linux, DWM on Windows — which
	 * publishes mode and accent only. `null` when neither answers.
	 */
	theme: OmarchyTheme | null;
	/**
	 * True when the console answers on more than this machine (`PUNKTFUNK_UI_BIND`). Settings says
	 * so out loud: an operator who never chose that should find out where they look at settings,
	 * not from `ss -ltnp`. `false` under `vite dev`, which stamps no bind.
	 */
	reachableFromNetwork: boolean;
}

export default defineEventHandler(async (event): Promise<UiConfig> => {
	// Read per request: `omarchy-theme-set` rewrites the file whenever the user switches theme,
	// and the client refetches on navigation, so the console follows without a restart. A
	// person's console takes neither: those are the owner's desktop, read with the admin token.
	const theme = event.context.person ? null : await resolveTheme();
	const reachableFromNetwork = !isLoopbackBind(boundAddress());
	const port = pluginOriginPort();
	if (port)
		return {
			pluginUi: "origin",
			pluginPort: port,
			theme,
			reachableFromNetwork,
		};
	// `import.meta.dev` is Nitro's build-time dev flag — false in every shipped build, so a
	// production bind failure can never resolve to the same-origin arrangement.
	if (import.meta.dev)
		return {
			pluginUi: "same-origin",
			pluginPort: null,
			theme,
			reachableFromNetwork,
		};
	return {
		pluginUi: "unavailable",
		pluginPort: null,
		theme,
		reachableFromNetwork,
	};
});

/** Omarchy › the host's own read › nothing. */
async function resolveTheme(): Promise<OmarchyTheme | null> {
	const omarchy = omarchyTheme();
	if (omarchy) return { ...omarchy, source: "omarchy" };
	const host = await hostTheme();
	if (!hasTheme(host)) return null;
	// Mode and accent only, so the console keeps its own surfaces. `background`/`foreground`
	// stay absent rather than blank: half a palette reads worse than none.
	return {
		mode: host.mode ?? "dark",
		...(host.accent ? { accent: host.accent } : {}),
		source: host.source ?? "host",
	};
}
