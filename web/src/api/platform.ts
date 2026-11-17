// The console's only platform gate (design/web-console-overhaul.md §3).
//
// Two questions, two sources, and they are not interchangeable:
//
//   IDENTITY — `HostInfo.os` — picks icons, docs anchors and platform wording.
//   CONTROL  — the `enforced` array of a settings plane — decides whether a control
//              is rendered at all.
//
// A control gated on identity is a second source of truth that drifts from the host, which alone
// knows what it acts on ("is there an AMD GPU?" is no proxy for "does `atiadlxx.dll` load?").
// Gate on `enforced`; a leak is then one host-side fix.
import { useGetDisplaySettings } from "@/api/gen/display/display";
import { useGetHostInfo } from "@/api/gen/host/host";
import { useGetSessionSettings } from "@/api/gen/session/session";

/** Which settings plane a field belongs to. */
export type Plane = "display" | "session";

export interface Platform {
	/** OS chain, generic → specific (`linux/fedora/bazzite`); `""` until `/host` answers. */
	os: string;
	/** The chain's first token: `windows` · `macos` · `linux`. */
	family: string;
	isWindows: boolean;
	isLinux: boolean;
	isMac: boolean;
	/** Whether this build acts on `field`. False hides the control — never disables it. */
	acts: (plane: Plane, field: string) => boolean;
	/** Whether the build acts on anything in `plane`. False hides the whole card. */
	actsAny: (plane: Plane) => boolean;
}

/**
 * Reads `enforced` with the three-way meaning the host's contract defines:
 * ABSENT is an older host that never sent the field (assume it acts — the compatible
 * reading), present-and-empty is "this build acts on none of it", and a present list is
 * exhaustive.
 */
const gate = (enforced: string[] | undefined) => ({
	acts: (field: string) => !enforced || enforced.includes(field),
	actsAny: () => !enforced || enforced.length > 0,
});

export function usePlatform(): Platform {
	const host = useGetHostInfo();
	const display = useGetDisplaySettings();
	const session = useGetSessionSettings();

	const os = host.data?.os ?? "";
	const family = os.split("/")[0] ?? "";
	const planes: Record<Plane, ReturnType<typeof gate>> = {
		display: gate(display.data?.enforced),
		session: gate(session.data?.enforced),
	};

	return {
		os,
		family,
		isWindows: family === "windows",
		// SteamOS is Linux, and `os` reports it as `linux/arch/steamos` — the family token
		// already carries that, so no alias list is needed here.
		isLinux: family === "linux",
		isMac: family === "macos",
		acts: (plane, field) => planes[plane].acts(field),
		actsAny: (plane) => planes[plane].actsAny(),
	};
}
