import { useQuery, useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { Check, ImageOff } from "lucide-react";
import { type FC, type FormEvent, useState } from "react";
import { useListLibraryMetadata } from "@/api/gen/library/library";
import {
	artVersion,
	isHttpUrl,
	type SourceImage,
	searchSource,
	setSourceMatch,
	sourceKey,
	useSourceImages,
	useSourceMatch,
	useSourceStatus,
} from "@/api/metadata";
import { usePlugins } from "@/api/plugins";
import { usePluginsReachable, useProfileView } from "@/api/seat";
import { QueryState } from "@/components/query-state";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
	Dialog,
	DialogContent,
	DialogDescription,
	DialogFooter,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Spinner } from "@/components/ui/spinner";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";
import { refreshLibrary } from "../helpers";
import { useSourceNames } from "../Sources";

export type ArtKind = "portrait" | "hero" | "header" | "logo";

export interface ArtSource {
	id: string;
	name: string;
}

/** A thrown call's own text, without the error's class name in front. */
const issueOf = (e: unknown): string =>
	e instanceof Error ? e.message : String(e);

/**
 * The Art & Metadata sources that can answer now: enabled, and running in a plugin runner the
 * console reaches. `loading` until both lists answered, so an empty list is a fact.
 */
export function useArtSources(enabled: boolean): {
	sources: ArtSource[];
	loading: boolean;
} {
	const reachable = usePluginsReachable() && enabled;
	const list = useListLibraryMetadata({ query: { enabled: reachable } });
	const plugins = usePlugins();
	const nameOf = useSourceNames();
	if (!reachable) return { sources: [], loading: false };
	const running = new Set((plugins.data ?? []).map((p) => p.id));
	return {
		sources: (list.data ?? [])
			.filter((s) => s.enabled && running.has(s.id))
			.map((s) => ({ id: s.id, name: nameOf(s.id) ?? s.id })),
		loading: list.isLoading || plugins.isLoading,
	};
}

/** A small 2:3 cover, or the empty frame of one. */
const Cover: FC<{ src?: string | null; className?: string }> = ({
	src,
	className,
}) => {
	const [failed, setFailed] = useState(false);
	return (
		<span
			className={cn(
				"flex aspect-[2/3] shrink-0 items-center justify-center overflow-hidden rounded-sm border bg-muted text-muted-foreground",
				className,
			)}
		>
			{src && !failed ? (
				<img
					src={src}
					alt=""
					loading="lazy"
					className="size-full object-cover"
					onError={() => setFailed(true)}
				/>
			) : (
				<ImageOff className="size-4" aria-hidden />
			)}
		</span>
	);
};

/**
 * Which game each source matched this title to, once for every slot. The match is the owner's
 * library's, so a profile's view only reads it.
 */
export const MatchCard: FC<{
	sources: ArtSource[];
	entryId: string;
	entryTitle: string;
}> = ({ sources, entryId, entryTitle }) => (
	<Card>
		<CardHeader>
			<CardTitle>{m.library_media_match()}</CardTitle>
			<p className="text-sm text-muted-foreground">
				{m.library_media_match_hint()}
			</p>
		</CardHeader>
		<CardContent>
			<ul className="divide-y">
				{sources.map((s) => (
					<MatchRow
						key={s.id}
						source={s}
						entryId={entryId}
						entryTitle={entryTitle}
					/>
				))}
			</ul>
		</CardContent>
	</Card>
);

const MatchRow: FC<{
	source: ArtSource;
	entryId: string;
	entryTitle: string;
}> = ({ source, entryId, entryTitle }) => {
	const qc = useQueryClient();
	const view = useProfileView();
	const status = useSourceStatus(source.id);
	const match = useSourceMatch(source.id, entryId);
	// The matched game's own cover: the same list the portrait slot offers.
	const covers = useSourceImages(source.id, entryId, "portrait");
	const [finding, setFinding] = useState(false);
	const [busy, setBusy] = useState(false);

	const unpin = async () => {
		setBusy(true);
		try {
			await setSourceMatch(source.id, entryId, null);
			await qc.invalidateQueries({ queryKey: sourceKey(source.id) });
			void refreshLibrary(qc);
		} catch (e) {
			toast.error(
				m.library_media_source_failed({
					source: source.name,
					issue: issueOf(e),
				}),
			);
		} finally {
			setBusy(false);
		}
	};

	const matched = match.data?.match;
	const cover = matched ? covers.data?.[0] : undefined;
	return (
		<li className="flex flex-wrap items-center gap-x-3 gap-y-2 py-3 first:pt-0 last:pb-0">
			<Cover src={cover?.thumb ?? cover?.url} className="w-10" />
			<div className="min-w-0 flex-1 basis-40 space-y-0.5">
				<p className="text-xs font-medium text-muted-foreground">
					{source.name}
				</p>
				{match.isError ? (
					<p role="alert" className="text-sm text-destructive">
						{m.library_media_source_failed({
							source: source.name,
							issue: issueOf(match.error),
						})}
					</p>
				) : (
					<p className="break-words text-sm">
						{match.isLoading
							? m.common_loading()
							: matched
								? m.library_media_matched({ label: matched.label })
								: m.library_media_no_match()}
					</p>
				)}
			</div>
			{!view && (
				<div className="flex flex-wrap gap-2">
					{match.data?.pinned && (
						<Button size="sm" variant="ghost" disabled={busy} onClick={unpin}>
							{m.library_media_unpin()}
						</Button>
					)}
					{status.data?.searchable && (
						<Button
							size="sm"
							variant="outline"
							onClick={() => setFinding(true)}
						>
							{m.library_media_change_match()}
						</Button>
					)}
				</div>
			)}
			{finding && (
				<MatchDialog
					source={source}
					entryId={entryId}
					entryTitle={entryTitle}
					current={matched?.key}
					onClose={() => setFinding(false)}
				/>
			)}
		</li>
	);
};

/**
 * "Wrong game?": search the source and pin one of what it finds. Opens on the title's own
 * search, so the likely games are listed before anything is typed.
 */
const MatchDialog: FC<{
	source: ArtSource;
	entryId: string;
	entryTitle: string;
	current?: string;
	onClose: () => void;
}> = ({ source, entryId, entryTitle, current, onClose }) => {
	const qc = useQueryClient();
	const [term, setTerm] = useState(entryTitle);
	const [asked, setAsked] = useState(entryTitle.trim());
	const [choosing, setChoosing] = useState<string | null>(null);
	const found = useQuery({
		queryKey: sourceKey(source.id, "search", entryId, asked),
		queryFn: () => searchSource(source.id, entryId, asked),
		enabled: asked !== "",
		retry: false,
		staleTime: 60_000,
	});

	const search = (e: FormEvent) => {
		e.preventDefault();
		setAsked(term.trim());
	};
	// A source that sends no cover for any game gets no column of empty frames.
	const covers = found.data?.some((c) => c.thumb) ?? false;
	const choose = async (key: string) => {
		setChoosing(key);
		try {
			await setSourceMatch(source.id, entryId, key);
		} catch (e) {
			toast.error(
				m.library_media_source_failed({
					source: source.name,
					issue: issueOf(e),
				}),
			);
			setChoosing(null);
			return;
		}
		// The match moved: what the source offers for every slot is another game's now.
		void qc.invalidateQueries({ queryKey: sourceKey(source.id) });
		void refreshLibrary(qc);
		onClose();
	};

	return (
		<Dialog open onOpenChange={(open) => !open && onClose()}>
			<DialogContent>
				<DialogHeader>
					<DialogTitle>
						{m.library_media_match_title({ source: source.name })}
					</DialogTitle>
					<DialogDescription>
						{m.library_media_match_body({ source: source.name })}
					</DialogDescription>
				</DialogHeader>
				<form onSubmit={search} className="flex gap-2">
					<Input
						aria-label={m.library_media_search()}
						value={term}
						autoFocus
						onChange={(e) => setTerm(e.target.value)}
					/>
					<Button
						size="input"
						type="submit"
						disabled={!term.trim() || found.isFetching}
					>
						{m.library_media_search()}
					</Button>
				</form>
				{found.isError ? (
					<p role="alert" className="text-sm text-destructive">
						{m.library_media_source_failed({
							source: source.name,
							issue: issueOf(found.error),
						})}
					</p>
				) : (
					<QueryState isLoading={found.isLoading} error={null}>
						{found.data?.length === 0 ? (
							<p className="text-sm text-muted-foreground">
								{m.library_media_no_results()}
							</p>
						) : (
							<ul className="-mx-2 flex flex-col gap-1">
								{found.data?.map((c) => (
									<li key={c.key}>
										<button
											type="button"
											disabled={choosing !== null}
											aria-current={c.key === current || undefined}
											onClick={() => choose(c.key)}
											className="flex min-h-11 w-full items-center gap-3 rounded-md p-2 text-left text-sm outline-none transition-colors hover:bg-primary/15 focus-visible:ring-2 focus-visible:ring-ring disabled:opacity-60"
										>
											{covers && <Cover src={c.thumb} className="w-12" />}
											<span className="min-w-0 flex-1 break-words font-medium">
												{c.label}
											</span>
											{choosing === c.key ? (
												<Spinner className="size-5 shrink-0" />
											) : (
												c.key === current && (
													<Badge variant="secondary" className="shrink-0">
														{m.library_media_match_current()}
													</Badge>
												)
											)}
										</button>
									</li>
								))}
							</ul>
						)}
					</QueryState>
				)}
				<DialogFooter>
					<Button variant="outline" onClick={onClose}>
						{m.common_cancel()}
					</Button>
				</DialogFooter>
			</DialogContent>
		</Dialog>
	);
};

/** The thumbnail frame and columns per slot, so a banner is not squeezed into a poster. */
const FRAME: Record<ArtKind, { cell: string; grid: string }> = {
	portrait: {
		cell: "aspect-[2/3]",
		grid: "grid-cols-3 @md:grid-cols-4 @2xl:grid-cols-6",
	},
	hero: {
		cell: "aspect-[96/31]",
		grid: "grid-cols-1 @md:grid-cols-2 @3xl:grid-cols-3",
	},
	header: {
		cell: "aspect-[92/43]",
		grid: "grid-cols-2 @md:grid-cols-3 @3xl:grid-cols-4",
	},
	logo: {
		cell: "aspect-[16/9]",
		grid: "grid-cols-2 @md:grid-cols-3 @3xl:grid-cols-4",
	},
};

/** What a slot shows now, as an offered image can be held against: its URL, or its `?v=`. */
export interface Shown {
	url?: string;
	version?: string | null;
}

/**
 * One source's images for a slot, the shown one marked. Says so when the source matched no
 * game, since then it has nothing for any slot.
 */
export const SourceImages: FC<{
	source: ArtSource;
	entryId: string;
	kind: ArtKind;
	shown: Shown;
	busy: boolean;
	onPick: (image: SourceImage) => void;
}> = ({ source, entryId, kind, shown, busy, onPick }) => {
	const match = useSourceMatch(source.id, entryId);
	const images = useSourceImages(source.id, entryId, kind);
	const urls = (images.data ?? []).map((i) => i.url);
	// Outside a secure context there is no digest, and no image is marked.
	const versions = useQuery({
		queryKey: ["art-versions", urls],
		queryFn: async () =>
			Object.fromEntries(
				await Promise.all(
					urls.map(async (u) => [u, await artVersion(u)] as const),
				),
			),
		enabled: !shown.url && !!shown.version && urls.length > 0,
		staleTime: Number.POSITIVE_INFINITY,
		retry: false,
	});
	const isShown = (url: string) =>
		shown.url
			? shown.url === url
			: !!shown.version && versions.data?.[url] === shown.version;

	const frame = FRAME[kind];
	return (
		<section className="space-y-2">
			<h4 className="text-xs font-medium text-muted-foreground">
				{source.name}
			</h4>
			{images.isError ? (
				<p role="alert" className="text-sm text-destructive">
					{m.library_media_source_failed({
						source: source.name,
						issue: issueOf(images.error),
					})}
				</p>
			) : (
				<QueryState isLoading={images.isLoading} error={null}>
					{images.data?.length === 0 ? (
						<p className="text-sm text-muted-foreground">
							{match.data && !match.data.match
								? m.library_media_unmatched({ source: source.name })
								: m.library_media_no_images()}
						</p>
					) : (
						<ul className={`grid gap-3 ${frame.grid}`}>
							{images.data?.map((img) => {
								const on = isShown(img.url);
								return (
									<li key={img.url}>
										<button
											type="button"
											disabled={busy}
											aria-pressed={on}
											aria-label={img.label ?? source.name}
											onClick={() => onPick(img)}
											className="group block w-full space-y-1 text-left outline-none"
										>
											<span
												className={cn(
													frame.cell,
													"relative block overflow-hidden rounded-md border bg-muted transition-shadow group-hover:ring-2 group-hover:ring-ring/60 group-focus-visible:ring-2 group-focus-visible:ring-ring",
													on && "ring-2 ring-primary group-hover:ring-primary",
												)}
											>
												{kind === "logo" && (
													<span className="absolute inset-0 bg-black/60" />
												)}
												<img
													src={img.thumb ?? img.url}
													alt=""
													loading="lazy"
													className={cn(
														"absolute inset-0 size-full",
														kind === "logo"
															? "object-contain p-3"
															: "object-cover",
													)}
												/>
												{on && (
													<span className="absolute top-1.5 right-1.5 flex size-5 items-center justify-center rounded-full bg-primary text-primary-foreground">
														<Check className="size-3.5" aria-hidden />
													</span>
												)}
											</span>
											{img.label && (
												<span
													className="block truncate text-xs text-muted-foreground tabular-nums"
													aria-hidden
												>
													{img.label}
												</span>
											)}
										</button>
									</li>
								);
							})}
						</ul>
					)}
				</QueryState>
			)}
		</section>
	);
};

/** A web address typed by hand, for a slot whose image no source offers. */
export const PasteImage: FC<{
	busy: boolean;
	onPick: (url: string) => void;
}> = ({ busy, onPick }) => {
	const [pasted, setPasted] = useState("");
	const url = pasted.trim();
	const submit = (e: FormEvent) => {
		e.preventDefault();
		if (!isHttpUrl(url)) return;
		onPick(url);
		setPasted("");
	};
	return (
		<form onSubmit={submit} className="max-w-xl space-y-2">
			<label htmlFor="art-paste" className="text-sm font-medium">
				{m.library_media_paste()}
			</label>
			<div className="flex gap-2">
				<Input
					id="art-paste"
					type="url"
					inputMode="url"
					value={pasted}
					placeholder="https://"
					onChange={(e) => setPasted(e.target.value)}
				/>
				<Button size="input" type="submit" disabled={busy || !isHttpUrl(url)}>
					{m.library_media_use()}
				</Button>
			</div>
		</form>
	);
};
