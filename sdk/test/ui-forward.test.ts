// A plugin without network serves its UI on a socket; the console dials a loopback port.
import { afterEach, describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { forwardUi, redirectUiServe } from "../src/ui-forward.js";

const serve = Bun.serve;
afterEach(() => {
	(Bun as { serve: typeof Bun.serve }).serve = serve;
});

describe("ui forward", () => {
	test("the first ephemeral loopback server answers on the forwarded port", async () => {
		const dir = fs.mkdtempSync(path.join(os.tmpdir(), "pf-ui-"));
		const socket = path.join(dir, "ui.sock");
		const forward = forwardUi(socket);
		redirectUiServe(forward.port, socket);
		const ui = Bun.serve({
			hostname: "127.0.0.1",
			port: 0,
			fetch: (req) =>
				new Response(`${new URL(req.url).pathname} ${req.headers.get("authorization")}`),
		});
		// A second one is the plugin's own business and keeps its real port.
		const other = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch: () => new Response("x") });
		try {
			expect(ui.port).toBe(forward.port);
			expect(other.port).not.toBe(forward.port);
			const res = await fetch(`http://127.0.0.1:${forward.port}/__health`, {
				headers: { authorization: "Bearer s" },
			});
			expect(await res.text()).toBe("/__health Bearer s");
		} finally {
			ui.stop(true);
			other.stop(true);
			forward.close();
			fs.rmSync(dir, { recursive: true, force: true });
		}
	});

	test("a fixed port or a non-loopback host is left alone", () => {
		redirectUiServe(1, "/nonexistent/ui.sock");
		const fixed = Bun.serve({ hostname: "0.0.0.0", port: 0, fetch: () => new Response("x") });
		try {
			expect(fixed.port).not.toBe(1);
		} finally {
			fixed.stop(true);
		}
	});
});
