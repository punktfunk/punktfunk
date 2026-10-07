// The kit's wire schemas are written by hand for their authoring docs; the SDK's are generated
// from the host's OpenAPI spec. `bun run typecheck` fails when the two stop naming the same
// fields, or when the kit accepts a value the host does not.
import { expect, test } from "bun:test";
import type { api } from "@punktfunk/host/core";
import type {
	Artwork,
	DetectHint,
	GameMeta,
	LaunchSpec,
	MetadataEntry,
	OnWindow,
	ProviderEntry,
} from "../src/wire.js";

/** The named keys: the generated types also carry a `[x: string]` index for unknown fields. */
type Known<T> = keyof {
	[K in keyof T as string extends K
		? never
		: number extends K
			? never
			: K]: T[K];
};
type SameKeys<A, B> = [Known<A>] extends [Known<B>]
	? [Known<B>] extends [Known<A>]
		? true
		: false
	: false;
type Host = api.ProviderEntryInput;

// Everything below compiles only while the pairs agree.
const sameKeys: [
	SameKeys<ProviderEntry, Host>,
	SameKeys<DetectHint, NonNullable<Host["detect"]>>,
	SameKeys<OnWindow, NonNullable<Host["on_window"]>>,
	SameKeys<MetadataEntry, api.MetadataEntryInput>,
	SameKeys<LaunchSpec, api.LaunchSpec>,
	SameKeys<Artwork, api.Artwork>,
	SameKeys<GameMeta, api.GameMeta>,
] = [true, true, true, true, true, true, true];
const accepted = {
	provider: (e: ProviderEntry): Host => e,
	metadata: (e: MetadataEntry): api.MetadataEntryInput => e,
};

test("kit wire schemas name the host's fields", () => {
	expect(sameKeys.every(Boolean)).toBe(true);
	expect(accepted.provider({ external_id: "x", title: "x" }).title).toBe("x");
});
