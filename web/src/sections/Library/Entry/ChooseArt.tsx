import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { type FC, type FormEvent, useState } from "react";
import {
	getGetLibraryQueryKey,
	useListLibraryMetadata,
} from "@/api/gen/library/library";
import {
	isHttpUrl,
	type SourceCandidate,
	type SourceImage,
	searchSource,
	setSourceMatch,
	sourceKey,
	useSourceImages,
	useSourceMatch,
	useSourceStatus,
} from "@/api/metadata";
import { usePlugins } from "@/api/plugins";
import { Button } from "@/components/ui/button";
import {
	Dialog,
	DialogContent,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { m } from "@/paraglide/messages";
import { useSourceNames } from "../Sources";

export type ArtKind = "portrait" | "hero" | "header" | "logo";

/** The thumbnail frame per slot, so a banner is not squeezed into a poster. */
const FRAME: Record<ArtKind, { cell: string; grid: string }> = {
	portrait: { cell: "aspect-[2/3]", grid: "grid-cols-3 sm:grid-cols-5" },
	hero: { cell: "aspect-[96/31]", grid: "grid-cols-1 sm:grid-cols-2" },
	header: { cell: "aspect-[92/43]", grid: "grid-cols-2 sm:grid-cols-3" },
	logo: { cell: "aspect-[16/9]", grid: "grid-cols-2 sm:grid-cols-3" },
};

/** One slot's image from any running Art & Metadata source, or a pasted URL. */
export const ChooseArtDialog: FC<{
	entryId: string;
	entryTitle: string;
	kind: ArtKind;
	slotLabel: string;
	onPick: (url: string) => void;
	onClose: () => void;
}> = ({ entryId, entryTitle, kind, slotLabel, onPick, onClose }) => {
	const list = useListLibraryMetadata();
	const plugins = usePlugins();
	const nameOf = useSourceNames();
	const [pasted, setPasted] = useState("");
	const running = new Set((plugins.data ?? []).map((p) => p.id));
	const sources = (list.data ?? []).filter(
		(s) => s.enabled && running.has(s.id),
	);
	const usePasted = (e: FormEvent) => {
		e.preventDefault();
		if (isHttpUrl(pasted.trim())) onPick(pasted.trim());
	};
	return (
		<Dialog open onOpenChange={(open) => !open && onClose()}>
			<DialogContent className="max-w-3xl">
				<DialogHeader>
					<DialogTitle>
						{m.library_media_choose_title({ slot: slotLabel })}
					</DialogTitle>
				</DialogHeader>
				<div className="space-y-6">
					{sources.length === 0 && (
						<p className="text-sm text-muted-foreground">
							{m.library_media_no_sources()}
						</p>
					)}
					{sources.map((s) => (
						<SourcePanel
							key={s.id}
							id={s.id}
							name={nameOf(s.id) ?? s.id}
							entryId={entryId}
							entryTitle={entryTitle}
							kind={kind}
							onPick={onPick}
						/>
					))}
					<form onSubmit={usePasted} className="space-y-2 border-t pt-4">
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
							<Button type="submit" disabled={!isHttpUrl(pasted.trim())}>
								{m.library_media_use()}
							</Button>
						</div>
					</form>
				</div>
			</DialogContent>
		</Dialog>
	);
};

/** One source: what it matched, "wrong game?", and its images for the slot. */
const SourcePanel: FC<{
	id: string;
	name: string;
	entryId: string;
	entryTitle: string;
	kind: ArtKind;
	onPick: (url: string) => void;
}> = ({ id, name, entryId, entryTitle, kind, onPick }) => {
	const qc = useQueryClient();
	const status = useSourceStatus(id);
	const match = useSourceMatch(id, entryId);
	const images = useSourceImages(id, entryId, kind);
	const [searching, setSearching] = useState(false);
	const [term, setTerm] = useState(entryTitle);
	const [results, setResults] = useState<SourceCandidate[] | null>(null);
	const [busy, setBusy] = useState(false);

	const failed = (e: unknown) =>
		toast.error(
			m.library_media_source_failed({ source: name, issue: String(e) }),
		);
	const choose = async (key: string | null) => {
		setBusy(true);
		try {
			await setSourceMatch(id, entryId, key);
			setSearching(false);
			setResults(null);
			await qc.invalidateQueries({ queryKey: sourceKey(id) });
			await qc.invalidateQueries({ queryKey: getGetLibraryQueryKey() });
		} catch (e) {
			failed(e);
		} finally {
			setBusy(false);
		}
	};
	const search = async (e: FormEvent) => {
		e.preventDefault();
		setBusy(true);
		try {
			setResults(await searchSource(id, entryId, term.trim()));
		} catch (err) {
			failed(err);
		} finally {
			setBusy(false);
		}
	};

	const matched = match.data?.match;
	return (
		<section className="space-y-3">
			<div className="flex flex-wrap items-center gap-2">
				<h3 className="font-medium">{name}</h3>
				{match.isSuccess && (
					<span className="text-sm text-muted-foreground">
						{matched
							? m.library_media_matched({ label: matched.label })
							: m.library_media_no_match()}
					</span>
				)}
				<div className="ml-auto flex gap-2">
					{match.data?.pinned && (
						<Button
							size="sm"
							variant="ghost"
							disabled={busy}
							onClick={() => choose(null)}
						>
							{m.library_media_unpin()}
						</Button>
					)}
					{status.data?.searchable && (
						<Button
							size="sm"
							variant="outline"
							aria-expanded={searching}
							onClick={() => setSearching((v) => !v)}
						>
							{m.library_media_change_match()}
						</Button>
					)}
				</div>
			</div>
			{searching && (
				<form onSubmit={search} className="flex gap-2">
					<Input
						aria-label={m.library_media_search()}
						value={term}
						onChange={(e) => setTerm(e.target.value)}
					/>
					<Button type="submit" disabled={busy || !term.trim()}>
						{m.library_media_search()}
					</Button>
				</form>
			)}
			{results &&
				(results.length === 0 ? (
					<p className="text-sm text-muted-foreground">
						{m.library_media_no_results()}
					</p>
				) : (
					<div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
						{results.map((c) => (
							<button
								key={c.key}
								type="button"
								disabled={busy}
								onClick={() => choose(c.key)}
								className="flex flex-col gap-1 rounded-md border p-2 text-left text-sm hover:bg-muted disabled:opacity-50"
							>
								{c.thumb && (
									<img
										src={c.thumb}
										alt=""
										className="aspect-[2/3] w-full rounded object-cover"
									/>
								)}
								<span className="line-clamp-2">{c.label}</span>
							</button>
						))}
					</div>
				))}
			{images.isError ? (
				<p className="text-sm text-destructive">
					{m.library_media_source_failed({
						source: name,
						issue: String(images.error),
					})}
				</p>
			) : (
				<ImageGrid images={images.data ?? []} kind={kind} onPick={onPick} />
			)}
		</section>
	);
};

const ImageGrid: FC<{
	images: SourceImage[];
	kind: ArtKind;
	onPick: (url: string) => void;
}> = ({ images, kind, onPick }) => {
	if (images.length === 0) {
		return (
			<p className="text-sm text-muted-foreground">
				{m.library_media_no_images()}
			</p>
		);
	}
	const frame = FRAME[kind];
	return (
		<div className={`grid gap-2 ${frame.grid}`}>
			{images.map((img) => (
				<button
					key={img.url}
					type="button"
					title={img.label}
					onClick={() => onPick(img.url)}
					className={`${frame.cell} overflow-hidden rounded-md border bg-muted hover:ring-2 hover:ring-ring`}
				>
					<img
						src={img.thumb ?? img.url}
						alt={img.label ?? ""}
						loading="lazy"
						className={`size-full ${kind === "logo" ? "object-contain p-2" : "object-cover"}`}
					/>
				</button>
			))}
		</div>
	);
};
