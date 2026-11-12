// Where plugin UIs live, from the server that knows.
//
// Plugin UIs are served from a DIFFERENT ORIGIN than the console (2026-08-05 review H-3): same
// scheme and host, its own port. The console has to build iframe and new-tab URLs against that
// origin, and the port has to come from the server — only it knows whether the listener bound.
import { useQuery } from "@tanstack/react-query";
import type { UiConfig } from "../../server/routes/_auth/ui-config.get";

/**
 * Deployment facts the console cannot infer.
 *
 * Polled, and it did not used to be: the ports genuinely cannot change without a server restart
 * (which reloads the page), so this was cached for the session — but the THEME on the same payload
 * changes whenever the user runs `omarchy-theme-set`, and a console that only asked at startup sat
 * in the old palette until someone reloaded it by hand. Polling and not pushing because the
 * console's SSE stream is a proxy of the HOST's, and the host reads nothing about themes.
 *
 * The interval does not run while the tab is in the background (TanStack's default), and the
 * refetch-on-focus this re-enables means switching theme and looking at the console is already
 * enough. One small local file read per tick.
 */
export const useUiConfig = () =>
	useQuery({
		queryKey: ["ui-config"],
		queryFn: async (): Promise<UiConfig> => {
			const r = await fetch("/_auth/ui-config", {
				credentials: "same-origin",
			});
			if (!r.ok) throw new Error(`ui-config ${r.status}`);
			return (await r.json()) as UiConfig;
		},
		refetchInterval: 2_000,
		retry: 2,
	});

/**
 * The origin serving plugin UIs, or `null` when there is none and the console must say so rather
 * than render a frame.
 *
 * Built from the CURRENT location's scheme and hostname, so it follows whatever address the
 * operator actually browsed to — an IP, an mDNS name, a hostname — and only the port differs. That
 * is the name the operator's certificate exception and the listener's `frame-ancestors` match.
 */
export function pluginOriginFrom(
	config: UiConfig | undefined,
): string | null | undefined {
	if (!config) return undefined; // still loading — render neither frame nor error
	if (config.pluginUi === "same-origin") return ""; // vite dev: relative URLs, one origin
	if (config.pluginUi === "origin" && config.pluginPort) {
		return `${window.location.protocol}//${window.location.hostname}:${config.pluginPort}`;
	}
	return null; // unavailable — the listener did not bind
}
