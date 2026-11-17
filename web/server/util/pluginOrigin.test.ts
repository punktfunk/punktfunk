// The `frame-ancestors` source the plugin origin names the console with.
//
// This exists because getting it wrong is SILENT on the server: the header is well-formed, every
// curl of the plugin origin returns 200, and the only symptom is that a browser quietly refuses to
// paint the frame (`ERR_BLOCKED_BY_RESPONSE`) — so the console shows an empty panel and the reason
// is only in devtools. An `http://host:47992` source for an `https://host:47992` console is the
// typical wrong answer.
import { describe, expect, test } from "bun:test";
import {
	frameAncestorSource,
	isLoopbackBind,
	isPluginUiPath,
} from "./pluginOrigin";

describe("frameAncestorSource", () => {
	test("uses the listener's scheme, NOT the request's", () => {
		// Nitro's localFetch synthesises a request with no TLS socket, so the request says
		// `http:` on an HTTPS listener. The listener's own scheme has to win, or the browser
		// refuses to frame the plugin.
		expect(
			frameAncestorSource({
				requestScheme: "http:",
				listenerScheme: "https",
				hostname: "192.168.1.21",
				port: 47992,
			}),
		).toBe("https://192.168.1.21:47992");
	});

	test("a plain-HTTP console (dev) still gets http", () => {
		expect(
			frameAncestorSource({
				requestScheme: "http:",
				listenerScheme: "http",
				hostname: "localhost",
				port: 3000,
			}),
		).toBe("http://localhost:3000");
	});

	test("x-forwarded-proto wins — only a proxy knows the browser's scheme", () => {
		expect(
			frameAncestorSource({
				forwardedProto: "https",
				requestScheme: "http:",
				listenerScheme: "http",
				hostname: "console.lan",
				port: 47992,
			}),
		).toBe("https://console.lan:47992");
	});

	test("a multi-hop x-forwarded-proto uses the first hop", () => {
		expect(
			frameAncestorSource({
				forwardedProto: "https, http",
				requestScheme: "http:",
				listenerScheme: "http",
				hostname: "console.lan",
				port: 47992,
			}),
		).toBe("https://console.lan:47992");
	});

	test("a junk x-forwarded-proto is ignored rather than echoed", () => {
		expect(
			frameAncestorSource({
				forwardedProto: "javascript:alert(1)",
				requestScheme: "http:",
				listenerScheme: "https",
				hostname: "192.168.1.21",
				port: 47992,
			}),
		).toBe("https://192.168.1.21:47992");
	});

	test("falls back to the request scheme when nothing is stamped", () => {
		expect(
			frameAncestorSource({
				listenerScheme: null,
				requestScheme: "https:",
				hostname: "host",
				port: 47992,
			}),
		).toBe("https://host:47992");
	});

	test("keeps whatever hostname the operator browsed to", () => {
		// The policy has to match their address bar, not a name we prefer.
		for (const hostname of ["192.168.1.21", "punktfunk.local", "deck"]) {
			expect(
				frameAncestorSource({
					requestScheme: "http:",
					listenerScheme: "https",
					hostname,
					port: 47992,
				}),
			).toBe(`https://${hostname}:47992`);
		}
	});
});

describe("isPluginUiPath", () => {
	// The two refusals that keep a plugin off the console's origin depend on this split.
	test("claims the plugin-UI prefix", () => {
		expect(isPluginUiPath("/plugin-ui")).toBe(true);
		expect(isPluginUiPath("/plugin-ui/")).toBe(true);
		expect(isPluginUiPath("/plugin-ui/rom-manager/index.html")).toBe(true);
	});

	test("claims nothing else — above all not /api", () => {
		expect(isPluginUiPath("/api/v1/status")).toBe(false);
		expect(isPluginUiPath("/")).toBe(false);
		expect(isPluginUiPath("/login")).toBe(false);
		// A near-miss must not be swept in by a loose startsWith.
		expect(isPluginUiPath("/plugin-uix")).toBe(false);
		expect(isPluginUiPath("/plugin-ui-admin")).toBe(false);
	});
});

describe("isLoopbackBind", () => {
	// Settings shows the network notice on `false`, so a false negative tells an operator their
	// console is private when the LAN can reach it. Everything not provably local reads as public.
	test("knows the addresses that answer only here", () => {
		expect(isLoopbackBind("127.0.0.1")).toBe(true);
		expect(isLoopbackBind("127.0.0.53")).toBe(true);
		expect(isLoopbackBind("::1")).toBe(true);
		expect(isLoopbackBind("localhost")).toBe(true);
		// No stamp at all: `vite dev`, where the notice would be noise.
		expect(isLoopbackBind(null)).toBe(true);
	});

	test("calls everything else reachable", () => {
		expect(isLoopbackBind("0.0.0.0")).toBe(false);
		expect(isLoopbackBind("::")).toBe(false);
		expect(isLoopbackBind("192.168.1.21")).toBe(false);
		expect(isLoopbackBind("100.64.0.3")).toBe(false);
	});
});
