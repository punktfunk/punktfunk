// A plugin's UI, embedded in the console (plugin-ui-surface §5). We probe the plugin's liveness
// first and only mount the iframe when it answers — otherwise the iframe would show the proxy's raw
// 502.
//
// The iframe is CROSS-ORIGIN: plugin UIs are served from their own origin (same scheme and host,
// its own port — see nitro-entry/bun-https.mjs and 2026-08-05 review H-3). The plugin can still talk
// to its own loopback REST with the operator's session, because that origin is same-SITE and the
// `SameSite=Lax` cookie reaches it; what it can no longer do is read or drive the console. It may
// still keep the address bar in sync by posting `{ type: "pf-ui:navigate", path }` to the parent —
// now verified against the plugin origin before it is honoured. The proxy-injected receiver asks
// for the console's palette with `pf-ui:theme-request`.
import { useQuery } from "@tanstack/react-query";
import { getRouteApi, useNavigate } from "@tanstack/react-router";
import { ExternalLink, Pin, PinOff, RefreshCw } from "lucide-react";
import { type FC, useEffect, useMemo, useRef } from "react";
import { pluginIcon, usePlugins } from "@/api/plugins";
import { useInstalledPlugins } from "@/api/store";
import { pluginOriginFrom, useUiConfig } from "@/api/uiConfig";
import { DocsLink } from "@/components/docs-link";
import { Button } from "@/components/ui/button";
import { useLocale } from "@/lib/i18n";
import { pluginPin, togglePin, usePins } from "@/lib/nav";
import { m } from "@/paraglide/messages";
import { TierBadge } from "@/sections/Store/TierBadge";

const route = getRouteApi("/plugins/$pluginId/$");

/** The palette names plugin-kit's theme.css declares that the console also themes. */
const THEME_TOKENS = [
	"--pf-brand",
	"--pf-brand-light",
	"--pf-highlight",
	"--background",
	"--foreground",
	"--card",
	"--card-foreground",
	"--popover",
	"--popover-foreground",
	"--muted",
	"--muted-foreground",
	"--secondary",
	"--secondary-foreground",
	"--accent",
	"--accent-foreground",
	"--border",
	"--input",
	"--ring",
	"--primary",
	"--primary-foreground",
	"--success",
	"--destructive",
	"--destructive-foreground",
];

/** Computed values, so the frame never re-derives the accent and desktop mixes in styles.css. */
function themeMessage() {
	const root = document.documentElement;
	const style = getComputedStyle(root);
	return {
		type: "pf-ui:theme",
		dark: root.classList.contains("dark"),
		tokens: Object.fromEntries(
			THEME_TOKENS.map((t) => [t, style.getPropertyValue(t).trim()]),
		),
	};
}

export const SectionPlugin: FC = () => {
	useLocale();
	const { pluginId, _splat } = route.useParams();
	const navigate = useNavigate();
	// This page is a plugin's only fixed address now that plugin UIs no longer inject a sidebar
	// entry — so the way back into the sidebar is here, on the page you are already on.
	const [pins, setPins] = usePins();
	const pin = pluginPin(pluginId);
	const isPinned = pins.includes(pin);
	const iframeRef = useRef<HTMLIFrameElement>(null);

	// Header metadata (title/version/icon) from the directory; falls back to the id.
	const { data: plugins } = usePlugins();
	const meta = plugins?.find((p) => p.id === pluginId);
	const Icon = pluginIcon(meta?.ui?.icon);
	const title = meta?.title ?? pluginId;

	// Provenance follows the plugin into its own page: an unverified plugin must stay visibly
	// unverified WHILE you use it, not only in the store listing. The store keys installations by
	// package, so the runtime id is matched through `plugin_id`.
	const { data: installed } = useInstalledPlugins();
	const provenance = installed?.find((p) => p.plugin_id === pluginId);

	// Where plugin UIs are served from. `undefined` = still resolving, `null` = the plugin listener
	// did not bind, so there is nowhere safe to render this and we say so instead of falling back to
	// the console's own origin — that fallback IS the vulnerability.
	const { data: uiConfig } = useUiConfig();
	const pluginOrigin = pluginOriginFrom(uiConfig);

	// Liveness: a 200 from /__health means the plugin is up.
	//
	// Two subtleties, both learned the hard way:
	//
	//  - A 200 is not enough. `fetch` follows redirects, so an expired session — where the gate
	//    answers 302 → /login → 200 HTML — looked exactly like a healthy plugin, and the console
	//    rendered its own login page inside the plugin's iframe. This now asks the CONSOLE origin,
	//    which probes the plugin server-side (the plugin origin is cross-origin to us and would need
	//    CORS to be readable from here) — and a bounced session is a plain 401, not HTML.
	//  - One failure must not be terminal. The runner is restarted at the end of every successful
	//    install, so a single missed probe is routine; giving up on the first one threw away
	//    whatever the operator had open in another plugin. Retry a few times, and keep probing on a
	//    slower beat while down so it recovers on its own.
	const health = useQuery({
		queryKey: ["plugin-health", pluginId],
		queryFn: async () => {
			const r = await fetch(`/_plugin-health/${pluginId}`, {
				credentials: "same-origin",
				redirect: "manual",
			});
			// `type === "opaqueredirect"` is the gate bouncing us to /login, not an answer.
			if (r.type === "opaqueredirect") throw new Error("session expired");
			if (!r.ok) throw new Error(`health ${r.status}`);
			return true;
		},
		retry: 2,
		refetchInterval: (q) => (q.state.status === "error" ? 5_000 : 20_000),
	});

	// Is the plugin ORIGIN reachable from this browser? Distinct from "is the plugin running".
	//
	// The console is served with the host's own self-signed certificate, and a browser stores a
	// certificate exception PER ORIGIN — including the port. So the operator having trusted
	// https://host:47992 says nothing about https://host:47993, and a certificate interstitial
	// cannot be shown (let alone accepted) inside an iframe: the frame would just sit blank, with no
	// way to fix it and nothing on screen explaining why.
	//
	// A `no-cors` probe distinguishes the two cases without needing CORS: the response is opaque and
	// unreadable either way, but a TLS failure REJECTS while an ordinary answer — even a 401 —
	// resolves. Rejection therefore means "this browser will not talk to that origin yet", which is
	// a one-time, fixable thing, so we say so and link to it.
	const reachable = useQuery({
		queryKey: ["plugin-origin-reachable", pluginOrigin],
		enabled: !!pluginOrigin,
		queryFn: async () => {
			await fetch(`${pluginOrigin}/plugin-ui/${pluginId}/__health`, {
				mode: "no-cors",
				cache: "no-store",
			});
			return true;
		},
		retry: 1,
		staleTime: 60_000,
	});

	// The frame must not be mounted until that probe has SETTLED.
	//
	// A firewalled port DROPs rather than refuses, so the probe does not fail fast — it hangs for the
	// browser's whole connect timeout. Mounting the iframe in the meantime puts an empty panel on
	// screen with nothing to explain it, and that is precisely what a closed 47993 looked like in the
	// field: a plugin page that "just doesn't load", with the real answer only in devtools. Waiting
	// costs a placeholder for the milliseconds a reachable origin takes, and buys the explanation.
	//
	// Only gate when the probe actually runs: under `vite dev` `pluginOrigin` is "" and the query is
	// disabled, which leaves it pending forever — gating on that would never render a frame at all.
	const originProbePending = !!pluginOrigin && reachable.isPending;

	// The iframe src is fixed at the initial deep-link path; the plugin's own in-app navigation drives
	// the console URL via postMessage (below), never the src — so there's no reload loop.
	// biome-ignore lint/correctness/useExhaustiveDependencies: intentionally pinned to the initial path
	const initialSrc = useMemo(
		() => `${pluginOrigin ?? ""}/plugin-ui/${pluginId}/${_splat ?? ""}`,
		[pluginId, pluginOrigin],
	);

	// Keep the console address bar in sync with the plugin's internal routing, and hand the frame
	// the console's palette when it asks and whenever the root's theme attributes change.
	useEffect(() => {
		// Empty `pluginOrigin` is the vite-dev same-origin arrangement.
		const expected = pluginOrigin || window.location.origin;
		const postTheme = () =>
			iframeRef.current?.contentWindow?.postMessage(themeMessage(), expected);
		const onMessage = (e: MessageEvent) => {
			if (e.source !== iframeRef.current?.contentWindow) return;
			// The frame is cross-origin, so this is a real check: only the plugin origin may drive
			// the console's address bar.
			if (e.origin !== expected) return;
			const data = e.data as { type?: string; path?: string };
			if (data?.type === "pf-ui:theme-request") postTheme();
			if (data?.type === "pf-ui:navigate" && typeof data.path === "string") {
				navigate({
					to: "/plugins/$pluginId/$",
					params: { pluginId, _splat: data.path.replace(/^\//, "") },
					replace: true,
				});
			}
		};
		window.addEventListener("message", onMessage);
		const observer = new MutationObserver(postTheme);
		observer.observe(document.documentElement, {
			attributeFilter: ["class", "style", "data-accent", "data-omarchy"],
		});
		return () => {
			window.removeEventListener("message", onMessage);
			observer.disconnect();
		};
	}, [pluginId, navigate, pluginOrigin]);

	return (
		<div className="flex h-[calc(100dvh-7rem)] min-h-[480px] flex-col gap-3 sm:h-[calc(100dvh-5rem)]">
			{/* Header strip: identity + open-in-new-tab (the plugin stands alone full-window too). */}
			{/* On a phone the title truncates and the two actions keep their icons only. */}
			<div className="flex min-w-0 items-center gap-3">
				<Icon className="size-5 shrink-0 text-muted-foreground" />
				<h1 className="min-w-0 truncate text-lg font-semibold">{title}</h1>
				{meta?.version && (
					<span className="hidden shrink-0 rounded bg-muted px-1.5 py-0.5 text-xs text-muted-foreground sm:inline">
						v{meta.version}
					</span>
				)}
				{provenance && (
					<TierBadge tier={provenance.tier} className="shrink-0" />
				)}
				<Button
					variant="ghost"
					size="sm"
					className="ml-auto shrink-0"
					aria-pressed={isPinned}
					aria-label={isPinned ? m.nav_unpin() : m.nav_pin()}
					title={isPinned ? m.nav_unpin() : m.nav_pin()}
					onClick={() => setPins(togglePin(pins, pin))}
				>
					{isPinned ? (
						<PinOff className="size-4" />
					) : (
						<Pin className="size-4" />
					)}
					<span className="hidden sm:inline">
						{isPinned ? m.nav_unpin() : m.nav_pin()}
					</span>
				</Button>
				{/* Full-window, on the PLUGIN origin. This link used to be the same escalation as the
				    iframe with no sandbox involved at all — a top-level document on the console origin,
				    holding the operator's session. It only stops being that because the origin moved,
				    which is why the fix could never have been a sandbox attribute. */}
				{pluginOrigin !== null && pluginOrigin !== undefined && (
					<a
						href={`${pluginOrigin}/plugin-ui/${pluginId}/`}
						target="_blank"
						rel="noreferrer"
						aria-label={m.plugin_open_new_tab()}
						title={m.plugin_open_new_tab()}
						className="inline-flex shrink-0 items-center gap-1.5 text-sm text-muted-foreground transition-colors hover:text-foreground"
					>
						<ExternalLink className="size-4" />
						<span className="hidden sm:inline">{m.plugin_open_new_tab()}</span>
					</a>
				)}
			</div>

			{pluginOrigin === null ? (
				<UnavailableCard />
			) : reachable.isError ? (
				<UntrustedOriginCard
					href={`${pluginOrigin}/plugin-ui/${pluginId}/`}
					onRetry={() => reachable.refetch()}
				/>
			) : health.isError ? (
				<OfflineCard title={title} onRetry={() => health.refetch()} />
			) : health.isSuccess &&
				!originProbePending &&
				pluginOrigin !== undefined ? (
				<iframe
					ref={iframeRef}
					src={initialSrc}
					title={title}
					className="w-full flex-1 rounded-lg border bg-card"
					// `allow-same-origin` is correct HERE and was the vulnerability BEFORE, because what
					// counts as "same origin" changed underneath it: the frame now loads from the plugin
					// origin, so this grants the plugin its OWN origin (storage, its own fetches) rather
					// than the console's. Removing it would give the frame an opaque origin instead,
					// which stops the SameSite=Lax session cookie and 302s every plugin asset to /login
					// — the dead end recorded in 2026-08-05 review H-3. Origin isolation is enforced by
					// the two listeners (nitro-entry/bun-https.mjs), not by this attribute.
					sandbox="allow-scripts allow-forms allow-popups allow-same-origin allow-modals"
					allow="fullscreen"
				/>
			) : (
				// Probing: a calm shimmer rather than a flash of empty frame.
				<div className="flex-1 animate-pulse rounded-lg border bg-card/50" />
			)}
		</div>
	);
};

/**
 * The plugin-UI listener did not bind, so there is no origin to render a plugin on.
 *
 * Deliberately a dead end rather than a fallback: serving the plugin on the console's own origin is
 * exactly the escalation the separate origin exists to prevent, so "the port is busy" must degrade
 * to "no plugin UIs", never to "plugin UIs, unsafely".
 */
const UnavailableCard: FC = () => (
	<div className="flex flex-1 items-center justify-center rounded-lg border border-dashed">
		<div className="flex min-w-0 max-w-md flex-col items-center gap-3 p-4 text-center sm:p-8">
			<h2 className="text-base font-semibold">
				{m.plugin_origin_unavailable_title()}
			</h2>
			<p className="text-sm text-muted-foreground">
				{m.plugin_origin_unavailable_hint()}{" "}
				<DocsLink path="plugins#troubleshooting" />
			</p>
		</div>
	</div>
);

/**
 * The plugin origin exists but this browser will not talk to it yet — almost always the host's
 * self-signed certificate not having been accepted for that PORT (exceptions are per origin), which
 * an iframe can never prompt for. One visit in a real tab fixes it for good.
 */
const UntrustedOriginCard: FC<{ href: string; onRetry: () => void }> = ({
	href,
	onRetry,
}) => (
	<div className="flex flex-1 items-center justify-center rounded-lg border border-dashed">
		<div className="flex min-w-0 max-w-md flex-col items-center gap-3 p-4 text-center sm:p-8">
			<h2 className="text-base font-semibold">
				{m.plugin_origin_untrusted_title()}
			</h2>
			<p className="text-sm text-muted-foreground">
				{m.plugin_origin_untrusted_hint()}{" "}
				<DocsLink path="plugins#troubleshooting" />
			</p>
			<Button asChild variant="outline" size="sm">
				<a href={href} target="_blank" rel="noreferrer">
					<ExternalLink className="size-4" />
					{m.plugin_origin_untrusted_open()}
				</a>
			</Button>
			<Button variant="ghost" size="sm" onClick={onRetry}>
				<RefreshCw className="size-4" />
				{m.plugin_retry()}
			</Button>
		</div>
	</div>
);

const OfflineCard: FC<{ title: string; onRetry: () => void }> = ({
	title,
	onRetry,
}) => (
	<div className="flex flex-1 items-center justify-center rounded-lg border border-dashed">
		<div className="flex min-w-0 max-w-md flex-col items-center gap-3 p-4 text-center sm:p-8">
			<h2 className="text-base font-semibold">{m.plugin_offline_title()}</h2>
			<p className="text-sm text-muted-foreground">
				<span className="font-medium text-foreground">{title}</span> —{" "}
				{m.plugin_offline_hint()}
			</p>
			{/* The exact runner commands, so the operator can act without leaving the page. */}
			<pre className="w-full overflow-x-auto rounded-md bg-muted p-3 text-left text-xs text-muted-foreground">
				<code>
					systemctl --user status punktfunk-scripting{"\n"}
					Get-ScheduledTask PunktfunkScripting{"  # Windows"}
				</code>
			</pre>
			<Button variant="outline" size="sm" onClick={onRetry}>
				<RefreshCw className="size-4" />
				{m.plugin_retry()}
			</Button>
		</div>
	</div>
);
