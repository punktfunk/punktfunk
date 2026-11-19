// Controllers on this PC: who a row offers, the name it shows, what it says under it, and its
// picture.
import { expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";
import type { DeskPad } from "@/api/gen/model/deskPad";
import type { ProfileAdmin } from "@/api/gen/model/profileAdmin";
import { baseLocale, overwriteGetLocale } from "@/paraglide/runtime";
import { PAD_ART } from "@/sections/Diagnostics/Controllers/padArt";
import {
	ART_BY_PRODUCT,
	ART_BY_VENDOR,
	DESK,
	holderOptions,
	holderValue,
	padArtKey,
	padName,
	padNote,
	tellApart,
} from "./deskPads";
import { DeskPadsCard } from "./deskPadsCard";

overwriteGetLocale(() => baseLocale);

const profile = (id: string, name: string, seat = true): ProfileAdmin => ({
	id,
	display_name: name,
	owner: false,
	home: "desktop",
	last_used_unix: 0,
	default: false,
	seat: seat ? { state: "ready", port: 9777, kind: "desktop" } : undefined,
});
const ENRICO = { ...profile("4f1c3a9b0e27", "Enrico", false), owner: true };
const ANNA = profile("0b7d5e2a91c4", "Anna", false);
const KID = profile("9a3f1c2b7e40", "Kid");
const MIA = profile("5a7c9e1b3d5f", "Mia");
const PROFILES = [ENRICO, ANNA, KID, MIA];

const pad = (key: string, more: Partial<DeskPad> = {}): DeskPad => ({
	key,
	name: "Xbox Wireless Controller",
	vendor_id: 0x045e,
	product_id: 0x0b13,
	present: true,
	state: "desk",
	...more,
});
const AT_DESK = pad("usb-045e-0b13-s-A1");
const GIVEN = pad("usb-045e-0b13-s-B2", { state: "live", profile: KID.id });
const PARKED = pad("usb-054c-0ce6-s-C3", {
	name: "DualSense",
	vendor_id: 0x054c,
	product_id: 0x0ce6,
	state: "parked",
	profile: MIA.id,
});
const GONE = pad("usb-28de-1205-s-D4", {
	name: "Steam Deck",
	vendor_id: 0x28de,
	product_id: 0x1205,
	present: false,
	state: "parked",
	profile: KID.id,
});

const values = (p: DeskPad) => holderOptions(p, PROFILES).map((o) => o.value);

test("a plugged-in pad offers this PC and each profile with a seat, by name", () => {
	expect(holderOptions(AT_DESK, PROFILES)).toEqual([
		{ value: DESK, label: "This PC" },
		{ value: KID.id, label: "Kid" },
		{ value: MIA.id, label: "Mia" },
	]);
	expect(holderValue(AT_DESK)).toBe(DESK);
	expect(holderValue(GIVEN)).toBe(KID.id);
});

test("profiles that share the owner's desktop are one seat, offered as the owner", () => {
	const shared = { state: "ready", port: 9779, kind: "shared" } as const;
	const owner = { ...ENRICO, seat: shared };
	const sharer = { ...ANNA, seat: shared };
	const offered = holderOptions(AT_DESK, [owner, sharer, KID]);
	expect(offered.map((o) => o.value)).toEqual([DESK, owner.id, KID.id]);
});

test("an unplugged pad can go back to this PC, never to another profile", () => {
	expect(values(GONE)).toEqual([DESK, KID.id]);
	expect(values({ ...GONE, state: "desk", profile: undefined })).toEqual([
		DESK,
	]);
});

test("a holder that lost its seat or its row stays selectable", () => {
	const noSeat = pad("usb-1", { state: "live", profile: ANNA.id });
	expect(values(noSeat)).toEqual([DESK, ANNA.id, KID.id, MIA.id]);
	const gone = pad("usb-2", { state: "live", profile: "ffffffffffff" });
	expect(holderOptions(gone, PROFILES).at(-1)).toEqual({
		value: "ffffffffffff",
		label: "ffffffffffff",
	});
});

test("a row says unplugged, or when a parked pad starts working, and nothing else", () => {
	expect(padNote(GONE, PROFILES)).toBe("Not plugged in");
	expect(padNote(PARKED, PROFILES)).toBe("Works once Mia's desktop starts.");
	expect(padNote(AT_DESK, PROFILES)).toBeNull();
	expect(padNote(GIVEN, PROFILES)).toBeNull();
});

test("twins get the last six characters of their key, others nothing", () => {
	const twin = pad("usb-045e-0b13-s-ZZ9999");
	const all = [AT_DESK, twin, PARKED];
	expect(tellApart(twin, all)).toBe("ZZ9999");
	expect(tellApart(AT_DESK, all)).toBe("3-s-A1");
	expect(tellApart(PARKED, all)).toBeNull();
	expect(tellApart(AT_DESK, [AT_DESK])).toBeNull();
});

test("a given name replaces the hardware's, and tells twins apart", () => {
	const named = pad("usb-045e-0b13-s-ZZ9999", { label: "Couch" });
	expect(padName(named)).toBe("Couch");
	expect(padName(AT_DESK)).toBe("Xbox Wireless Controller");
	expect(tellApart(named, [AT_DESK, named])).toBeNull();
	expect(tellApart(AT_DESK, [AT_DESK, named])).toBeNull();
	const text = card([AT_DESK, named]).replace(/<[^>]*>/g, "|");
	expect(text).toContain("|Couch|");
	expect(text.match(/\|Xbox Wireless Controller\|/g)).toHaveLength(2);
	expect(text).not.toContain("ZZ9999");
});

test("makers map to a drawing the console has; others to the plain icon", () => {
	for (const key of [
		...Object.values(ART_BY_PRODUCT),
		...Object.values(ART_BY_VENDOR),
	]) {
		expect(PAD_ART[key]).toBeDefined();
	}
	expect(padArtKey(PARKED)).toBe("dualsense");
	expect(padArtKey(GONE)).toBe("steamdeck");
	expect(padArtKey(AT_DESK)).toBe("xboxone");
	expect(padArtKey(pad("k", { vendor_id: 0x057e, product_id: 0x2009 }))).toBe(
		"switchpro",
	);
	expect(
		padArtKey(pad("k", { vendor_id: 0x28de, product_id: 0x9999 })),
	).toBeNull();
	expect(
		padArtKey(pad("k", { vendor_id: 0x0079, product_id: 0x0006 })),
	).toBeNull();
});

const card = (pads: DeskPad[]) =>
	renderToStaticMarkup(
		<DeskPadsCard
			pads={pads}
			profiles={PROFILES}
			busyKey={null}
			onGive={() => {}}
			onRename={() => {}}
		/>,
	);

test("the card names the rule, each pad, its note and its twins' tell", () => {
	const twin = pad("usb-045e-0b13-s-ZZ9999");
	const html = card([AT_DESK, twin, PARKED, GONE]);
	expect(html).toContain("Controllers on this PC");
	expect(html).toContain("A controller plugged in here plays at this PC.");
	expect(html.match(/>Used by</g)).toHaveLength(4);
	expect(html).toContain("Works once Mia&#x27;s desktop starts.");
	expect(html).toContain("Not plugged in");
	// What a person reads: the tell, never the whole key.
	const text = html.replace(/<[^>]*>/g, "|");
	expect(text).toContain("ZZ9999");
	expect(text).not.toContain("usb-");
	expect(html).toContain('aria-hidden="true"');
});
