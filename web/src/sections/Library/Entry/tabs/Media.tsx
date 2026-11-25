import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { ImageOff } from "lucide-react";
import { type FC, type ReactNode, useState } from "react";
import { useSetLibraryArtPick } from "@/api/gen/library/library";
import { versionOf } from "@/api/metadata";
import { useAs, useProfileView, whose } from "@/api/seat";
import { LAUNCHER_ICONS, LauncherIcon } from "@/components/launcher-icon";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { apiErrorMessage } from "@/lib/errors";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";
import { OwnBadge } from "../../GameCard";
import { refreshLibrary } from "../../helpers";
import { useSourceNames } from "../../Sources";
import {
	type ArtKind,
	MatchCard,
	PasteImage,
	type Shown,
	SourceImages,
	useArtSources,
} from "../ArtSources";
import { Group, ReadRow, TextField } from "../fields";
import type { FormState } from "../model";
import type { TabProps } from "./types";

const KINDS: ArtKind[] = ["portrait", "hero", "header", "logo"];

const SLOTS: Record<
	ArtKind,
	{ label: () => string; help: () => string; frame: string }
> = {
	portrait: {
		label: m.library_media_portrait,
		help: m.library_media_portrait_help,
		frame: "aspect-[2/3]",
	},
	hero: {
		label: m.library_media_hero,
		help: m.library_media_hero_help,
		frame: "aspect-[96/31]",
	},
	header: {
		label: m.library_media_header,
		help: m.library_media_header_help,
		frame: "aspect-[92/43]",
	},
	logo: {
		label: m.library_media_logo,
		help: m.library_media_logo_help,
		frame: "aspect-[16/9]",
	},
};

const NO_ICON = "none";

/** What the preview shows: a URL straight from the browser, a stored local path through the
 * host's art proxy, and nothing for a local path typed since the last save. */
export function previewSrc(
	kind: ArtKind,
	draft: FormState,
	baseline: FormState,
	proxied: string | null | undefined,
	readOnly: boolean,
): string | null | undefined {
	if (readOnly) return proxied ?? null;
	const v = draft[kind].trim();
	if (!v) return null;
	if (/^(https?:|data:)/.test(v)) return v;
	return v === baseline[kind].trim() ? (proxied ?? null) : undefined;
}

/**
 * One slot's picture in its frame. While `src` loads, its thumbnail or the picture before it
 * stays underneath, so a pick never blanks the frame. A logo sits on `backdrop`, the hero it is
 * drawn over, or on a dark ground.
 */
const Preview: FC<{
	src: string | null | undefined;
	thumb?: string;
	backdrop?: string | null;
	frame: string;
	contain: boolean;
	selected?: boolean;
}> = ({ src, thumb, backdrop, frame, contain, selected }) => {
	const [loaded, setLoaded] = useState<string | null>(null);
	const [failed, setFailed] = useState<string | null>(null);
	const broken = src != null && failed === src;
	const fit = contain ? "object-contain p-3" : "object-cover";
	const under = src && loaded !== src ? (thumb ?? loaded) : null;
	return (
		<span
			className={cn(
				frame,
				"relative flex w-full items-center justify-center overflow-hidden rounded-md border bg-muted text-muted-foreground transition-shadow group-hover:ring-2 group-hover:ring-ring/60 group-focus-visible:ring-2 group-focus-visible:ring-ring",
				selected && "ring-2 ring-primary group-hover:ring-primary",
			)}
		>
			{src && !broken ? (
				<>
					{contain && backdrop && (
						<img
							src={backdrop}
							alt=""
							className="absolute inset-0 size-full object-cover"
						/>
					)}
					{contain && <span className="absolute inset-0 bg-black/50" />}
					{under && (
						<img
							src={under}
							alt=""
							className={`absolute inset-0 size-full ${fit}`}
						/>
					)}
					<img
						key={src}
						src={src}
						alt=""
						className={`absolute inset-0 size-full ${fit}`}
						// A cached picture is complete before React listens for its load.
						ref={(el) => {
							if (el?.complete && el.naturalWidth > 0) setLoaded(src);
						}}
						onLoad={() => setLoaded(src)}
						onError={() => setFailed(src)}
					/>
				</>
			) : (
				<span className="flex flex-col items-center gap-1 p-2 text-center text-xs">
					<ImageOff className="size-4" aria-hidden />
					{broken
						? m.library_entry_art_failed()
						: src === undefined
							? m.library_entry_art_after_save()
							: m.library_entry_art_none()}
				</span>
			)}
		</span>
	);
};

/** A pick the host took, shown before the library lists it. `url: null` is a pick undone. */
interface Picked {
	url: string | null;
	/** The slot's art as the library listed it when the pick was made. */
	was: string | null;
}

/**
 * The game's four pictures as a stage, and the slot in hand: every Art & Metadata source's
 * images for that slot, the shown one marked. On an entry a plugin owns a pick is stored by the
 * host at once; on the operator's own entry it goes into the draft like a typed URL. In a
 * profile's view the pick is its own, marked, with one action back to the owner's. A person's
 * own console shows the art and offers no pick.
 */
export const MediaTab: FC<TabProps> = ({
	draft,
	set,
	readOnly,
	entry,
	baseline,
}) => {
	const qc = useQueryClient();
	const pick = useSetLibraryArtPick();
	const nameOf = useSourceNames();
	const view = useProfileView();
	const as = useAs();
	// The Art & Metadata sources are the box's plugins: a person's own console picks from none.
	const picks = !view?.person && entry !== null;
	const { sources, loading } = useArtSources(picks);
	const [kind, setKind] = useState<ArtKind>("portrait");
	const [picked, setPicked] = useState<Partial<Record<ArtKind, Picked>>>({});
	// A picked image's thumbnail, by its URL: already in the browser when the pick is made.
	const [thumbs, setThumbs] = useState<Record<string, string>>({});
	const filled = entry?.filled ?? {};
	const mine = new Set(entry?.profile?.picks ?? []);
	const usable = !readOnly || picks;

	/** The slot as the page shows it now. */
	const slot = (k: ArtKind) => {
		const listed = entry?.art[k] ?? null;
		// A picked URL holds until the page is left. An undone pick holds until the library moves.
		const over = picked[k];
		const live =
			over && (over.url !== null || over.was === listed) ? over : undefined;
		const own = previewSrc(k, draft, baseline, listed, readOnly);
		const from = filled[k];
		// A source fills an empty slot of the operator's own entry too; show what it put there.
		const borrowed = own === null && from !== undefined && from !== "pick";
		const src = live?.url ? live.url : borrowed ? listed : own;
		const byPick = live ? live.url !== null : from === "pick";
		const hers = view !== null && (live ? live.url !== null : mine.has(k));
		const shown: Shown = live?.url
			? { url: live.url }
			: !readOnly && draft[k].trim()
				? { url: draft[k].trim() }
				: { version: versionOf(listed) };
		return {
			src,
			shown,
			hers,
			/** The owner's own pick, which Reset undoes. */
			reset: readOnly && !view && byPick,
			caption: hers
				? null
				: byPick && readOnly
					? m.library_media_picked()
					: from !== undefined && from !== "pick" && (readOnly || borrowed)
						? m.library_media_from({ source: nameOf(from) ?? from })
						: null,
		};
	};

	const store = async (k: ArtKind, url: string | null, thumb?: string) => {
		if (url && thumb) setThumbs((t) => ({ ...t, [url]: thumb }));
		if (!readOnly) {
			set(k, url ?? "");
			return;
		}
		if (!entry) return;
		const before = picked[k];
		setPicked((p) => ({ ...p, [k]: { url, was: entry.art[k] ?? null } }));
		try {
			await pick.mutateAsync({
				id: entry.id,
				data: { kind: k, url },
				params: as,
			});
			void refreshLibrary(qc);
		} catch (e) {
			setPicked((p) => ({ ...p, [k]: before }));
			toast.error(apiErrorMessage(e) ?? m.library_media_pick_failed());
		}
	};

	const active = SLOTS[kind];
	const now = slot(kind);
	const hero = slot("hero").src;
	return (
		<>
			{entry && picks && sources.length > 0 && (
				<MatchCard
					sources={sources}
					entryId={entry.id}
					entryTitle={entry.title}
				/>
			)}
			<Group title={m.library_media_art()}>
				<div className="@container/art">
					{/* Side by side where there is room: a pick lands in the stage beside it. */}
					<div
						className={cn(
							"grid gap-x-6 gap-y-4",
							usable && "@3xl/art:grid-cols-[minmax(0,20rem)_minmax(0,1fr)]",
						)}
					>
						<div className="grid max-w-md grid-cols-[5fr_6fr] items-start gap-4 self-start [grid-template-areas:'portrait_header''portrait_logo''hero_hero']">
							{KINDS.map((k) => {
								const { label, frame } = SLOTS[k];
								const s = slot(k);
								const preview = (
									<Preview
										src={s.src}
										thumb={s.src ? thumbs[s.src] : undefined}
										backdrop={k === "logo" ? hero : undefined}
										frame={frame}
										contain={k === "logo"}
										selected={usable && k === kind}
									/>
								);
								return (
									<div
										key={k}
										className="flex min-w-0 flex-col gap-2"
										style={{ gridArea: k }}
									>
										{usable ? (
											<button
												type="button"
												aria-pressed={k === kind}
												aria-label={label()}
												onClick={() => setKind(k)}
												className="group rounded-md outline-none"
											>
												{preview}
											</button>
										) : (
											preview
										)}
										<div className="flex min-h-6 flex-wrap items-center gap-x-2 gap-y-1">
											<span className="text-sm font-medium">{label()}</span>
											{s.hers && (
												<OwnBadge
													label={
														whose(view?.name ?? "", view?.person === true).own
													}
												/>
											)}
											{s.caption && (
												<span className="text-xs text-muted-foreground">
													{s.caption}
												</span>
											)}
										</div>
									</div>
								);
							})}
						</div>
						{usable && (
							<div className="@container min-w-0 space-y-4 border-t pt-4 @3xl/art:border-t-0 @3xl/art:pt-0">
								{readOnly ? (
									<SlotHead label={active.label()}>
										{/* A profile undoes only its own pick; the owner's is the owner's. */}
										{view && now.hers && (
											<Button
												size="sm"
												variant="ghost"
												disabled={pick.isPending}
												onClick={() => store(kind, null)}
											>
												{m.profile_view_use_owner_art({
													owner: view.ownerName,
												})}
											</Button>
										)}
										{now.reset && (
											<Button
												size="sm"
												variant="ghost"
												disabled={pick.isPending}
												onClick={() => store(kind, null)}
											>
												{m.library_media_reset()}
											</Button>
										)}
									</SlotHead>
								) : (
									<div className="max-w-xl">
										<TextField
											id={kind}
											label={active.label()}
											value={draft[kind]}
											onChange={(v) => set(kind, v)}
											help={active.help()}
											type="url"
										/>
									</div>
								)}
								{entry && picks && sources.length > 0 && (
									// Beside the stage the list scrolls in place, so the slots stay in view.
									<div className="space-y-4 @3xl/art:-mx-1 @3xl/art:max-h-[34rem] @3xl/art:overflow-y-auto @3xl/art:p-1">
										{sources.map((s) => (
											<SourceImages
												key={s.id}
												source={s}
												entryId={entry.id}
												kind={kind}
												shown={now.shown}
												busy={pick.isPending}
												onPick={(img) => store(kind, img.url, img.thumb)}
											/>
										))}
									</div>
								)}
								{picks && !loading && sources.length === 0 && (
									<p className="text-sm text-muted-foreground">
										{m.library_media_no_sources()}
									</p>
								)}
								{readOnly && (
									<PasteImage
										busy={pick.isPending}
										onPick={(url) => store(kind, url)}
									/>
								)}
							</div>
						)}
					</div>
				</div>
			</Group>
			{/* A mark means something on a launcher tile; a game with none has nothing to show. */}
			{(!readOnly || draft.icon || draft.isLauncher) && (
				<Group
					title={m.library_entry_icon()}
					help={m.library_entry_icon_help()}
				>
					<div className="flex items-center gap-4">
						<div className="flex size-14 shrink-0 items-center justify-center rounded-md border bg-muted text-muted-foreground [&>svg]:size-8">
							<LauncherIcon icon={draft.icon || null} />
						</div>
						{readOnly ? (
							<ReadRow label={m.library_entry_icon()} value={draft.icon} />
						) : (
							<div className="w-full max-w-60 space-y-2">
								<Label htmlFor="entry-icon" className="sr-only">
									{m.library_entry_icon()}
								</Label>
								<Select
									value={draft.icon || NO_ICON}
									onValueChange={(v) => set("icon", v === NO_ICON ? "" : v)}
								>
									<SelectTrigger id="entry-icon" size="sm">
										<SelectValue />
									</SelectTrigger>
									<SelectContent>
										<SelectItem value={NO_ICON}>
											{m.library_entry_icon_none()}
										</SelectItem>
										{Object.keys(LAUNCHER_ICONS).map((token) => (
											<SelectItem key={token} value={token}>
												{token.charAt(0).toUpperCase() + token.slice(1)}
											</SelectItem>
										))}
									</SelectContent>
								</Select>
							</div>
						)}
					</div>
				</Group>
			)}
		</>
	);
};

/** The slot in hand by name, and what undoes its pick. */
const SlotHead: FC<{ label: string; children: ReactNode }> = ({
	label,
	children,
}) => (
	<div className="flex min-h-8 flex-wrap items-center justify-between gap-2">
		<h4 className="font-medium">{label}</h4>
		<div className="flex flex-wrap gap-2">{children}</div>
	</div>
);
