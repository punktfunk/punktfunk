import { expect, test } from "bun:test";
import { type InfiniteData, QueryClient } from "@tanstack/react-query";
import { getGetLibraryPageQueryKey } from "@/api/gen/library/library";
import type { LibraryPage } from "@/api/gen/model/libraryPage";
import type { OperatorGameEntry } from "@/api/gen/model/operatorGameEntry";
import { dropTitle, isOwnId, ownEntry } from "./helpers";

const KID = "9a3f1c2b7e40";
const OWN = "own:3f9a0c1b2d4e";

test("only a desktop's minted id is a profile's own game", () => {
	expect(isOwnId(OWN)).toBe(true);
	expect(isOwnId("custom:3f9a0c1b2d4e")).toBe(false);
	expect(isOwnId("own:3F9A0C1B2D4E")).toBe(false);
	expect(isOwnId("own:3f9a")).toBe(false);
});

test("an own game draws the box's pictures, and a cover on the desktop through the desktop", () => {
	const row = {
		id: OWN,
		title: "Speedrun build",
		art: {
			portrait: "/home/kid/Pictures/cover.png",
			hero: "https://img.example/hero.jpg",
			header: "https://img.example/header.jpg",
		},
	};
	const listed: OperatorGameEntry = {
		id: OWN,
		title: "Speedrun build",
		store: "custom",
		art: { header: `/api/v1/library/art/${OWN}/header?v=1&as=${KID}` },
		own: true,
	};
	expect(ownEntry(listed, row, KID).art).toEqual({
		portrait: `/api/v1/profiles/${KID}/proxy/library/art/${OWN}/portrait?v=%2Fhome%2Fkid%2FPictures%2Fcover.png`,
		hero: "https://img.example/hero.jpg",
		header: `/api/v1/library/art/${OWN}/header?v=1&as=${KID}`,
	});
	// Not listed yet: the stored row stands in, still as her own.
	const fresh = ownEntry(undefined, { id: OWN, title: "New" }, KID);
	expect([fresh.title, fresh.store, fresh.own, fresh.art]).toEqual([
		"New",
		"custom",
		true,
		{},
	]);
});

test("a dropped title leaves every loaded page and the count", () => {
	const qc = new QueryClient();
	const game = (id: string) => ({ id, title: id, store: "custom", art: {} });
	const page = (ids: string[], total: number): LibraryPage => ({
		items: ids.map(game),
		total,
		platforms: [],
	});
	const pages = [...getGetLibraryPageQueryKey({ as: KID }), "pages"];
	qc.setQueryData<InfiniteData<LibraryPage>>(pages, {
		pages: [page(["a", "b"], 3), page([OWN], 3)],
		pageParams: [undefined, "c"],
	});
	const one = getGetLibraryPageQueryKey({ id: OWN, limit: 1, as: KID });
	qc.setQueryData<LibraryPage>(one, page([OWN], 1));
	const other = getGetLibraryPageQueryKey({ limit: 1 });
	const untouched = page(["a"], 1);
	qc.setQueryData<LibraryPage>(other, untouched);

	dropTitle(qc, OWN);

	const after = qc.getQueryData<InfiniteData<LibraryPage>>(pages);
	expect(after?.pages.map((p) => [p.items.map((g) => g.id), p.total])).toEqual([
		[["a", "b"], 2],
		[[], 2],
	]);
	expect(qc.getQueryData<LibraryPage>(one)).toEqual(page([], 0));
	expect(qc.getQueryData(other)).toBe(untouched);
});
