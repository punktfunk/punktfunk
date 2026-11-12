// The console refuses a plugin page path without its credential segment, so the base every API
// call is built on must keep that whole first segment.
import { afterEach, expect, mock, test } from "bun:test";

// React is a peer the kit's own install leaves out; this helper never touches it.
mock.module("react", () => ({ useEffect() {}, useState() {} }));
const { resolvePluginBase } = await import("../src/react/index.js");

const g = globalThis as { window?: unknown };
const at = (pathname: string) => {
	g.window = { location: { pathname } };
};
afterEach(() => {
	delete g.window;
});

test("the base keeps the page's credential segment", () => {
	const page = `/plugin-ui/rom-manager-mzx8w0-${"0".repeat(32)}`;
	at(`${page}/sources`);
	expect(resolvePluginBase()).toBe(page);
	at(`${page}/`);
	expect(resolvePluginBase()).toBe(page);
});

test("a standalone page has no base", () => {
	at("/sources");
	expect(resolvePluginBase()).toBe("");
});
