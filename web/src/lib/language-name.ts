/**
 * A language's name in that language: `Deutsch`, `日本語`, `繁體中文`. The tag itself when the
 * browser has no name for it. The first letter is capitalised, as engines differ on `español`.
 */
export function languageName(tag: string): string {
	try {
		const name = new Intl.DisplayNames([tag], { type: "language" }).of(tag);
		if (name && name !== tag) {
			return name.charAt(0).toLocaleUpperCase(tag) + name.slice(1);
		}
	} catch {
		// No `Intl.DisplayNames`, or a tag it rejects: the code stands in.
	}
	return tag;
}
