import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import {
	carriesCommandExecution,
	UNPRIVILEGED_LAUNCH_KINDS,
} from "./command-execution";

const launch = (kind: unknown) => ({ launch: { kind, value: "x" } });

describe("command-execution gate", () => {
	test("matches the host's unprivileged launch kinds", () => {
		const repo = join(import.meta.dir, "..", "..", "..");
		const rust = readFileSync(
			join(repo, "crates/host/punktfunk-host/src/library/custom.rs"),
			"utf8",
		);
		const body = rust.match(
			/const UNPRIVILEGED_LAUNCH_KINDS: &\[&str\] = &\[([^\]]*)\]/,
		)?.[1];
		expect(body).toBeDefined();
		const host = [...(body ?? "").matchAll(/"([^"]+)"/g)].map((m) => m[1]);
		expect([...UNPRIVILEGED_LAUNCH_KINDS].sort()).toEqual(host.sort());
	});

	test("prep or a privileged kind asks", () => {
		expect(carriesCommandExecution({ prep: [{ do: "x" }] })).toBe(true);
		expect(carriesCommandExecution(launch("command"))).toBe(true);
	});

	test("a kind the host has not listed asks", () => {
		expect(carriesCommandExecution(launch("future_kind"))).toBe(true);
		expect(carriesCommandExecution({ launch: {} })).toBe(true);
	});

	test("listed kinds and no launch pass", () => {
		for (const kind of UNPRIVILEGED_LAUNCH_KINDS)
			expect(carriesCommandExecution(launch(kind))).toBe(false);
		expect(carriesCommandExecution({ launch: null, prep: [] })).toBe(false);
		expect(carriesCommandExecution(null)).toBe(false);
	});
});
