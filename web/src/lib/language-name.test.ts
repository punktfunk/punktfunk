import { expect, test } from "bun:test";
import { languageName } from "./language-name";

test("a language is named in itself", () => {
	expect(languageName("en")).toBe("English");
	expect(languageName("de")).toBe("Deutsch");
	expect(languageName("ja")).toBe("日本語");
	expect(languageName("zh-Hant")).toBe("繁體中文");
	expect(languageName("zh-Hans")).toBe("简体中文");
});

test("a Latin name starts with a capital in every engine", () => {
	expect(languageName("es")).toBe("Español");
	expect(languageName("fr")).toBe("Français");
});

test("a tag with no name stays as it is", () => {
	expect(languageName("xx")).toBe("xx");
	expect(languageName("not a tag")).toBe("not a tag");
});
