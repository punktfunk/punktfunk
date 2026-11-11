import { describe, expect, it } from "bun:test";
import { mkdtempSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loadEnvFiles, parseEnvFile } from "./env-files.mjs";

describe("parseEnvFile", () => {
	it("reads the lines the host and the password file write", () => {
		const text = [
			"# written by the host",
			"PUNKTFUNK_MGMT_URL=https://127.0.0.1:47995",
			"  PUNKTFUNK_MGMT_TOKEN = abc123  ",
			"PUNKTFUNK_UI_PASSWORD_HASH='$argon2id$v=19$m=65536$salt$hash'",
			'PUNKTFUNK_UI_PASSWORD="two words"',
			"; PUNKTFUNK_UI_SECRET=commented",
			"LD_PRELOAD=/tmp/evil.so",
			"not a setting",
			"",
		].join("\n");
		expect(parseEnvFile(text)).toEqual({
			PUNKTFUNK_MGMT_URL: "https://127.0.0.1:47995",
			PUNKTFUNK_MGMT_TOKEN: "abc123",
			PUNKTFUNK_UI_PASSWORD_HASH: "$argon2id$v=19$m=65536$salt$hash",
			PUNKTFUNK_UI_PASSWORD: "two words",
		});
	});
});

describe("loadEnvFiles", () => {
	const dir = mkdtempSync(join(tmpdir(), "pf-env-"));
	const file = (name: string, body: string) => {
		const path = join(dir, name);
		writeFileSync(path, body);
		return path;
	};

	it("lets a file beat the unit's value and a later file an earlier one", () => {
		const env: Record<string, string> = {
			PUNKTFUNK_MGMT_URL: "https://127.0.0.1:1",
			PORT: "47992",
			PUNKTFUNK_ENV_FILES: [
				file("a", "PUNKTFUNK_MGMT_URL=https://127.0.0.1:2\n"),
				join(dir, "missing"),
				file("b", "PUNKTFUNK_MGMT_URL=https://127.0.0.1:3\nPORT=1\n"),
			].join(":"),
		};
		loadEnvFiles(env);
		expect(env.PUNKTFUNK_MGMT_URL).toBe("https://127.0.0.1:3");
		expect(env.PORT).toBe("47992");
	});

	it("follows one key alone when asked", () => {
		const env: Record<string, string> = {
			PUNKTFUNK_MGMT_TOKEN: "kept",
			PUNKTFUNK_ENV_FILES: file(
				"c",
				"PUNKTFUNK_MGMT_TOKEN=new\nPUNKTFUNK_MGMT_URL=https://127.0.0.1:4\n",
			),
		};
		loadEnvFiles(env, ["PUNKTFUNK_MGMT_URL"]);
		expect(env.PUNKTFUNK_MGMT_TOKEN).toBe("kept");
		expect(env.PUNKTFUNK_MGMT_URL).toBe("https://127.0.0.1:4");
	});

	it("reads a link as the console's own account would", () => {
		// The point of reading here: a link the account can't read through is just skipped.
		const link = join(dir, "link");
		symlinkSync(join(dir, "nowhere"), link);
		const env: Record<string, string> = { PUNKTFUNK_ENV_FILES: link };
		loadEnvFiles(env);
		expect(Object.keys(env)).toEqual(["PUNKTFUNK_ENV_FILES"]);
	});

	it("does nothing where no file is named", () => {
		const env: Record<string, string> = {};
		loadEnvFiles(env);
		expect(env).toEqual({});
	});
});
