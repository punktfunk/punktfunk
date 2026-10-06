// Which library entries carry a command the host runs as its own user. The BFF gates those writes
// behind the console password (server/util/libraryConfirm.ts); the entry editor asks for the
// password on the same rule. Pure, so both sides import it.

/**
 * Launch kinds the host builds a command for from a validated value. Mirrors
 * `UNPRIVILEGED_LAUNCH_KINDS` in crates/punktfunk-host/src/library/custom.rs; a test pins the two.
 * Any other kind is privileged, so a kind the host adds prompts until it lands here.
 */
export const UNPRIVILEGED_LAUNCH_KINDS: readonly string[] = [
	"steam_appid",
	"steam_ui",
	"launcher_ui",
	"lutris_id",
	"heroic",
	"epic",
	"gog",
	"aumid",
	"xbox",
	"playnite",
	"uplay",
	"amazon",
	"battlenet",
	"ea",
	"rockstar",
	"exec",
	"emulator",
	"desktop_id",
	"gamebar",
];

/** The shape the gate inspects; everything else about the entry is none of its business. */
export interface EntryLike {
	prep?: unknown;
	launch?: { kind?: unknown } | null;
}

/** Does this entry carry `prep` or a launch kind the host treats as operator-only? */
export function carriesCommandExecution(
	entry: EntryLike | null | undefined,
): boolean {
	if (!entry || typeof entry !== "object") return false;
	if (Array.isArray(entry.prep) && entry.prep.length > 0) return true;
	if (entry.launch == null) return false;
	const kind = entry.launch.kind;
	return typeof kind !== "string" || !UNPRIVILEGED_LAUNCH_KINDS.includes(kind);
}
