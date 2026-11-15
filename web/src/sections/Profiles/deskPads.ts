// The pure side of **Controllers on this PC**: who a controller can go to, the picture it gets,
// and what its row says under the name.
import type { DeskPad } from "@/api/gen/model/deskPad";
import type { ProfileAdmin } from "@/api/gen/model/profileAdmin";
import { m } from "@/paraglide/messages";

/** The select's value for **This PC**. A profile id is hex, so it never collides. */
export const DESK = "desk";

export type HolderOption = { value: string; label: string };

/** The profile a controller is given to; `null` while this PC has it. */
export const holderOf = (pad: DeskPad): string | null =>
	pad.state === "desk" ? null : (pad.profile ?? null);

/** The select's value for `pad`. */
export const holderValue = (pad: DeskPad): string => holderOf(pad) ?? DESK;

/** `id`'s name, or the id itself for a profile the list no longer has. */
const nameOf = (id: string, profiles: readonly ProfileAdmin[]): string =>
	profiles.find((p) => p.id === id)?.display_name ?? id;

/**
 * Whether a controller can be given to `p`: it has a seat of its own. Profiles that share the
 * owner's desktop sit on one seat, so the owner stands for all of them.
 */
export const canHold = (p: ProfileAdmin): boolean =>
	p.seat != null && (p.seat.kind !== "shared" || p.owner);

/**
 * What the row's select offers: **This PC**, then each profile that can hold a controller. One
 * that isn't plugged in can't be given away, so it offers only its holder.
 */
export function holderOptions(
	pad: DeskPad,
	profiles: readonly ProfileAdmin[],
): HolderOption[] {
	const holder = holderOf(pad);
	const ids = profiles
		.filter((p) => p.id === holder || (pad.present && canHold(p)))
		.map((p) => p.id);
	if (holder && !ids.includes(holder)) ids.push(holder);
	return [
		{ value: DESK, label: m.profiles_pads_desk() },
		...ids.map((id) => ({ value: id, label: nameOf(id, profiles) })),
	];
}

/** The muted line under the name: unplugged, or waiting for its profile's desktop. */
export function padNote(
	pad: DeskPad,
	profiles: readonly ProfileAdmin[],
): string | null {
	if (!pad.present) return m.profiles_pads_absent();
	const holder = holderOf(pad);
	return pad.state === "parked" && holder
		? m.profiles_pads_parked({ name: nameOf(holder, profiles) })
		: null;
}

/** The end of the key, when another controller has the same name; else `null`. */
export const tellApart = (
	pad: DeskPad,
	all: readonly DeskPad[],
): string | null =>
	all.some((o) => o.key !== pad.key && o.name === pad.name)
		? pad.key.slice(-6)
		: null;

/** The drawing for a USB product: `PAD_ART`'s keys. */
export const ART_BY_PRODUCT: Record<string, string> = {
	"054c:0ce6": "dualsense",
	"054c:0df2": "dualsenseedge",
	"054c:05c4": "dualshock4",
	"054c:09cc": "dualshock4",
	"054c:0ba0": "dualshock4",
	"28de:1205": "steamdeck",
	"28de:1102": "steamcontroller",
	"28de:1142": "steamcontroller",
	"28de:1105": "steamcontroller",
	"28de:1106": "steamcontroller",
	"28de:1302": "steamcontroller2",
	"28de:1303": "steamcontroller2",
	"28de:1304": "steamcontroller2puck",
	"28de:1305": "steamcontroller2puck",
	"045e:028e": "xbox360",
	"045e:0291": "xbox360",
	"045e:02a1": "xbox360",
	"045e:0719": "xbox360",
	"045e:02e3": "xboxelite",
	"045e:0b00": "xboxelite",
	"045e:0b05": "xboxelite",
	"057e:2069": "switch2pro",
};

/** The drawing for a maker's other products. Valve has none: a Valve product not above is new. */
export const ART_BY_VENDOR: Record<string, string> = {
	"054c": "dualsense",
	"045e": "xboxone",
	"057e": "switchpro",
};

const hex = (n: number) => n.toString(16).padStart(4, "0");

/** The `PAD_ART` key for `pad`, or `null` for a maker without one: the row draws a plain icon. */
export function padArtKey(pad: DeskPad): string | null {
	const vendor = hex(pad.vendor_id);
	return (
		ART_BY_PRODUCT[`${vendor}:${hex(pad.product_id)}`] ??
		ART_BY_VENDOR[vendor] ??
		null
	);
}
