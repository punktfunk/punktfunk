// A plugin's UI, embedded in the console (plugin-ui-surface §5). We probe the plugin's liveness
// first and only mount the iframe when it answers — otherwise the iframe would show the proxy's raw
// 502.
//
// Plugin UIs are served from their own listener (nitro-entry/bun-https.mjs), and every page there
// is sandboxed into an opaque origin of its own. The health probe hands back the page's path, whose
// first segment is the page's credential; relative URLs carry it to the plugin's own REST. The frame
// keeps the address bar in sync by posting `{ type: "pf-ui:navigate", path }` to the parent, and the
// proxy-injected receiver asks for the console's palette with `pf-ui:theme-request`.
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
import {
	frameAction,
	healthFailure,
	healthIntervalMs,
	pluginBody,
} from "./pluginFrame";

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
	// The page is a plugin's only fixed address, so the pin back into the sidebar lives here.
	const [pins, setPins] = usePins();
	const pin = pluginPin(pluginId);
	const isPinned = pins.includes(pin);
	const pinLabel = isPinned ? m.nav_unpin() : m.nav_pin();
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

	// The console probes the plugin server-side and gets back the page path. A redirect or non-2xx
	// is a failure, so a bounced session never reads as a healthy plugin. One miss is routine while
	// the runner restarts: retry, then keep probing.
	const health = useQuery({
		queryKey: ["plugin-health", pluginId],
		queryFn: async () => {
			const r = await fetch(`/_plugin-health/${pluginId}`, {
				credentials: "same-origin",
				redirect: "manual",
			});
			const failure = healthFailure(r);
			if (failure) throw new Error(failure);
			return ((await r.json()) as { path: string }).path;
		},
		retry: 2,
		refetchInterval: (q) => healthIntervalMs(q.state.status),
	});

	// Whether this browser talks to the plugin origin, as opposed to whether the plugin runs.
	// A certificate exception is stored per origin, port included, and an iframe cannot show the
	// interstitial. A `no-cors` fetch rejects on a TLS failure and resolves on any answer.
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

	// The frame mounts after the origin probe settles: a firewalled port drops packets and hangs
	// for the connect timeout, which would leave an empty panel with no explanation.
	const body = pluginBody(pluginOrigin, reachable, health);

	// The latest page path; before the first health answer, the bare path the listener refuses.
	const pagePath = health.data ?? `/plugin-ui/${pluginId}/`;

	// The iframe src stays at the initial deep-link path and the first page path it got, which
	// lives as long as the session. The plugin's navigation drives the console URL by postMessage,
	// never the src, so the frame does not reload.
	// biome-ignore lint/correctness/useExhaustiveDependencies: intentionally pinned to the initial path
	const initialSrc = useMemo(
		() => `${pluginOrigin ?? ""}${pagePath}${_splat ?? ""}`,
		[pluginId, pluginOrigin, health.data === undefined],
	);

	// Keeps the console address bar in step with the plugin's routing, and hands the frame the
	// console palette when it asks and whenever the root's theme attributes change. The frame's
	// opaque origin can't be named as a target, so the palette, which holds nothing secret, goes
	// to "*".
	useEffect(() => {
		const postTheme = () =>
			iframeRef.current?.contentWindow?.postMessage(themeMessage(), "*");
		const onMessage = (e: MessageEvent) => {
			const action = frameAction(
				e.source === iframeRef.current?.contentWindow,
				e.data,
			);
			if (action?.kind === "theme") postTheme();
			if (action?.kind === "navigate") {
				navigate({
					to: "/plugins/$pluginId/$",
					params: { pluginId, _splat: action.splat },
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
	}, [pluginId, navigate]);

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
					aria-label={pinLabel}
					title={pinLabel}
					onClick={() => setPins(togglePin(pins, pin))}
				>
					{isPinned ? (
						<PinOff className="size-4" />
					) : (
						<Pin className="size-4" />
					)}
					<span className="hidden sm:inline">{pinLabel}</span>
				</Button>
				{/* Full-window, on the plugin origin: never a top-level page on the console's. */}
				{pluginOrigin != null && (
					<a
						href={`${pluginOrigin}${pagePath}`}
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

			{body === "unavailable" ? (
				<UnavailableCard />
			) : body === "untrusted" ? (
				<UntrustedOriginCard
					href={`${pluginOrigin}${pagePath}`}
					onRetry={() => reachable.refetch()}
				/>
			) : body === "offline" ? (
				<OfflineCard title={title} onRetry={() => health.refetch()} />
			) : body === "frame" ? (
				<iframe
					ref={iframeRef}
					src={initialSrc}
					title={title}
					className="w-full flex-1 rounded-lg border bg-card"
					// No `allow-same-origin`: the page gets an opaque origin, as the listener's CSP
					// `sandbox` also forces. Links it opens leave the sandbox, like any other tab.
					sandbox="allow-scripts allow-forms allow-popups allow-popups-to-escape-sandbox allow-modals"
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
