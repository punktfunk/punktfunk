import { describe, expect, test } from "bun:test";
import { platformName } from "./platform";

describe("platformName", () => {
	test("names a known id", () => {
		expect(platformName("macos")).toBe("macOS");
		expect(platformName("android")).toBe("Android");
		expect(platformName("web")).toBeTruthy();
	});

	test("an unknown id, or an Object key, has no name", () => {
		expect(platformName("horizon")).toBeUndefined();
		expect(platformName("constructor")).toBeUndefined();
		expect(platformName(null)).toBeUndefined();
	});
});
