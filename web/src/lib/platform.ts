import { m } from "@/paraglide/messages";

/**
 * Display names of platform ids: a plugin catalog's (`linux | windows | macos`) and a paired
 * device's (the OS its client was built for). Proper nouns, so not messages; `macos` is spelled
 * the way Apple spells it.
 */
const NAMES: Record<string, string> = {
	linux: "Linux",
	windows: "Windows",
	macos: "macOS",
	ios: "iOS",
	tvos: "tvOS",
	visionos: "visionOS",
	android: "Android",
	freebsd: "FreeBSD",
};

/** The name of a platform id; `undefined` for one this console does not know. */
export function platformName(
	id: string | null | undefined,
): string | undefined {
	if (id === "web") return m.devices_platform_web();
	// `hasOwn`: an id is the client's word, and `constructor` must not read Object's.
	return id && Object.hasOwn(NAMES, id) ? NAMES[id] : undefined;
}
