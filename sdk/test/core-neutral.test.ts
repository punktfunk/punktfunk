// `@punktfunk/host/core` must be importable from a browser, which means nothing reachable from
// it may name a `node:` module or a Bun global. A bundler would tell you this at the consumer's
// build — after publishing. This tells you here.
//
// A static walk rather than a bundling smoke: it needs no bundler, it is deterministic, and the
// thing it checks is textual. The one blind spot — a `node:` import behind a dynamic `import()`
// string — is not a pattern this codebase uses.

import { describe, expect, it } from "bun:test";
import * as fs from "node:fs";
import * as path from "node:path";

const SRC = path.resolve(import.meta.dir, "../src");

const importsOf = (file: string): string[] => {
	const text = fs.readFileSync(file, "utf8");
	const specifiers: string[] = [];
	// `import x from "…"`, `import type … from "…"`, `export { … } from "…"` (the braces may span
	// lines, which is why newlines are allowed up to the `from`), and `import("…")`.
	const re = /(?:^|\n)\s*(?:import|export)\s[^"']*?from\s*["']([^"']+)["']|import\(\s*["']([^"']+)["']\s*\)/g;
	for (const m of text.matchAll(re)) specifiers.push(m[1] ?? m[2] ?? "");
	return specifiers.filter(Boolean);
};

/** Every file reachable from `entry`, plus every bare specifier seen on the way. */
const walk = (entry: string): { files: Set<string>; bare: Set<string> } => {
	const files = new Set<string>();
	const bare = new Set<string>();
	const todo = [entry];
	while (todo.length) {
		const file = todo.pop()!;
		if (files.has(file)) continue;
		files.add(file);
		for (const spec of importsOf(file)) {
			if (spec.startsWith(".")) {
				// Emitted specifiers say `.js`; the sources are `.ts`.
				const target = path.resolve(path.dirname(file), spec.replace(/\.js$/, ".ts"));
				if (fs.existsSync(target)) todo.push(target);
			} else {
				bare.add(spec);
			}
		}
	}
	return { files, bare };
};

describe("@punktfunk/host/core", () => {
	it("reaches no node: or bun: module", () => {
		const { bare, files } = walk(path.join(SRC, "core.ts"));
		const platform = [...bare].filter((s) => /^(node:|bun:|bun$)/.test(s));
		expect(platform).toEqual([]);
		// And it must not have quietly pulled in the Node half by another name.
		const names = [...files].map((f) => path.basename(f));
		for (const nodeOnly of ["config.ts", "runner.ts", "runner-linux.ts", "runner-windows.ts", "runner-cli.ts", "plugins.ts", "ui.ts", "log-ship.ts"]) {
			expect(names).not.toContain(nodeOnly);
		}
	});

	it("still reaches the generated client, the service and the credential kinds", () => {
		const { files } = walk(path.join(SRC, "core.ts"));
		const names = [...files].map((f) => path.basename(f));
		for (const wanted of ["punktfunk.ts", "client.ts", "credential.ts", "api.ts", "http.ts", "sse.ts", "wire.ts"]) {
			expect(names).toContain(wanted);
		}
	});

	it("the Node entries are still the Node entries", () => {
		// The point of the split is not to make the SDK browser-only. `config.ts` must remain
		// reachable from the default entry, or `connect()` stops resolving token files.
		const { files } = walk(path.join(SRC, "index.ts"));
		expect([...files].map((f) => path.basename(f))).toContain("config.ts");
	});
});
