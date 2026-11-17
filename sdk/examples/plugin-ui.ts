// ── Example · console-hosted plugin UI ────────────────────────────────────────────────────────
// A plugin that surfaces a UI inside the punktfunk web console (plugin-ui-surface design). It
// serves a page + a live SSE feed on a loopback port behind a per-boot secret; `servePluginUi`
// registers it, and the console proxies to it and adds a "Demo UI" nav entry — no second password,
// no second port for the operator to learn. This also exercises the streaming path end-to-end:
// the SSE feed must arrive through the console's reverse proxy unbuffered.
//
// Run under the scripting runner, or directly for a quick look:  bun examples/plugin-ui.ts
import {
	connect,
	definePlugin,
	type Punktfunk,
	servePluginUi,
} from "../src/index.js";

// A tiny self-contained page: a heading and a list that prepends one line per SSE tick. The
// EventSource URL is RELATIVE (`./events`) so it resolves under the console proxy prefix.
const PAGE = `<!doctype html><meta charset=utf-8><title>Demo UI</title>
<style>body{font:15px system-ui;margin:2rem;color:#e5e7eb;background:#0b0b0f}li{opacity:.9}</style>
<h1>punktfunk plugin UI demo</h1><p>Live ticks (proxied SSE):</p><ul id=log></ul>
<script>
const log = document.getElementById("log");
new EventSource("./events").onmessage = (e) => {
  const li = document.createElement("li"); li.textContent = e.data; log.prepend(li);
};
</script>`;

// The plugin's dynamic handler. Paths arrive prefix-stripped (the console proxy removed
// `/plugin-ui/<page segment>`), so we match `/` and `/events` directly.
const handle = (req: Request): Response | undefined => {
	const { pathname } = new URL(req.url);
	if (pathname === "/events") {
		// A ticking SSE stream — the thing the proxy must forward unbuffered.
		let n = 0;
		const body = new ReadableStream({
			start(controller) {
				const enc = new TextEncoder();
				const timer = setInterval(() => {
					controller.enqueue(
						enc.encode(`data: tick ${++n} @ ${new Date().toISOString()}\n\n`),
					);
				}, 1000);
				req.signal.addEventListener("abort", () => {
					clearInterval(timer);
					controller.close();
				});
			},
		});
		return new Response(body, {
			headers: { "content-type": "text/event-stream", "cache-control": "no-cache" },
		});
	}
	if (pathname === "/" || pathname === "/index.html") {
		return new Response(PAGE, { headers: { "content-type": "text/html; charset=utf-8" } });
	}
	return undefined; // fall through → 404
};

const plugin = definePlugin({
	name: "demo-ui",
	main: async (pf) => {
		const ui = await servePluginUi(pf, {
			id: "demo-ui",
			title: "Demo UI",
			icon: "puzzle",
			fetch: handle,
		});
		console.log(`[demo-ui] serving on ${ui.url} — open the console's "Demo UI" nav entry`);
		// Run until asked to stop, then deregister cleanly (abrupt kills fall back to lease expiry).
		await new Promise<void>((resolve) => {
			process.once("SIGINT", resolve);
			process.once("SIGTERM", resolve);
		});
		await ui.close();
	},
});

export default plugin;

// Allow a direct `bun examples/plugin-ui.ts` run outside the managed runner.
if (import.meta.main) {
	const pf = await connect();
	await (plugin.main as (pf: Punktfunk) => Promise<void>)(pf);
	pf.close();
}
