import { Link } from "@tanstack/react-router";
import { Eye, EyeOff, Trash2 } from "lucide-react";
import { type FC, useState } from "react";
import type { Download } from "@/api/gen/model/download";
import type { OperatorGameEntry } from "@/api/gen/model/operatorGameEntry";
import { LauncherIcon } from "@/components/launcher-icon";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { m } from "@/paraglide/messages";
import { isOperatorOwned, storeLabel } from "./helpers";
import { InstallBadge } from "./Install";

export interface GameCardProps {
	game: OperatorGameEntry;
	onDelete: () => void;
	deleting: boolean;
	/** Hide this title from every play surface, or bring it back. */
	onToggleHidden: () => void;
	/** This card's hide/un-hide is in flight — only this one disables. */
	hiding: boolean;
	/** A source's display name by id (`useSourceNames`). */
	nameOf?: (id: string) => string | undefined;
	/** The title's download, while it has one. */
	download?: Download;
}

/**
 * A poster tile that opens the entry's page. The cover prefers the 2:3 portrait capsule; on a
 * load error it falls back to the wide header, then to a text placeholder. Every entry can be
 * hidden, custom entries deleted; the controls sit beside the link, never inside it.
 */
export const GameCard: FC<GameCardProps> = ({
	game,
	onDelete,
	deleting,
	onToggleHidden,
	hiding,
	nameOf,
	download,
}) => {
	// Every store can be hidden: the titles most worth hiding are the ones the operator cannot
	// edit. The host keys the setting by entry id and never needs to own the entry.
	const hidden = game.hidden === true;
	// Only the operator's own entries delete; the host refuses a provider-synced one (409).
	const isCustom = isOperatorOwned(game);
	// Track which sources have failed so the <img> can step down portrait → header → placeholder.
	const [failed, setFailed] = useState<Record<string, boolean>>({});

	const candidates = [game.art.portrait, game.art.header].filter(
		(u): u is string => !!u && !failed[u],
	);
	const src = candidates[0];
	// A launcher tile ships no cover art by design (its own icon is square and this frame is 2:3),
	// so the brand mark IS its poster. Only when there is no artwork at all: a plugin that does send
	// a cover has out-voted the token.
	const mark = !src && game.icon ? <LauncherIcon icon={game.icon} /> : null;

	return (
		<Card className="group relative overflow-hidden">
			<Link
				to="/library/$gameId"
				params={{ gameId: game.id }}
				className="block rounded-[inherit] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
			>
				<div className="relative aspect-[2/3] bg-muted">
					{/* Dim the ARTWORK only — never the badges or the buttons layered over it. A hidden
				    card is the sole place the title can be brought back, so its controls have to stay
				    at full contrast while the poster reads as "not in play". */}
					{src ? (
						<img
							src={src}
							alt={game.title}
							loading="lazy"
							decoding="async"
							className={`size-full object-cover${hidden ? " opacity-30" : ""}`}
							onError={() => setFailed((prev) => ({ ...prev, [src]: true }))}
						/>
					) : mark ? (
						// The mark carries the tile on its own — the launcher's name is already
						// directly below the frame, so repeating it here would just crowd the glyph.
						<div
							className={`flex size-full items-center justify-center p-8 text-muted-foreground${
								hidden ? " opacity-30" : ""
							}`}
						>
							<div className="w-full max-w-24 [&>svg]:size-full">{mark}</div>
						</div>
					) : (
						<div
							className={`flex size-full items-center justify-center p-3 text-center text-sm font-medium text-muted-foreground${
								hidden ? " opacity-30" : ""
							}`}
						>
							{game.title}
						</div>
					)}
					<div className="absolute left-2 top-2 flex flex-wrap gap-1">
						<Badge
							variant={isCustom ? "secondary" : "outline"}
							className="bg-background/80 backdrop-blur"
						>
							{storeLabel(game.store, nameOf)}
						</Badge>
						{/* Platform badge — "PC" is implied by every installed store, so only
					    non-PC platforms (the emulation case) earn a second badge. */}
						{game.platform && game.platform.toUpperCase() !== "PC" && (
							<Badge
								variant="outline"
								className="bg-background/80 backdrop-blur"
							>
								{game.platform}
							</Badge>
						)}
						{/* Who owns this entry, when it isn't the operator. Not when the owner is the store
					    the first badge already names: "Steam", then "via Steam". */}
						{game.provider && game.provider !== game.store && (
							<Badge
								variant="outline"
								className="bg-background/80 backdrop-blur"
							>
								{m.library_owned_by({
									provider: nameOf?.(game.provider) ?? game.provider,
								})}
							</Badge>
						)}
						{/* Says WHY this poster is faded. Without it a dimmed tile reads as a broken cover
					    or a still-loading image rather than a deliberate setting. */}
						{hidden && (
							<Badge
								variant="secondary"
								className="bg-background/90 backdrop-blur"
							>
								{m.library_hidden_badge()}
							</Badge>
						)}
					</div>
					<div className="absolute bottom-2 left-2">
						<InstallBadge
							{...(game.install ? { install: game.install } : {})}
							{...(download ? { download } : {})}
						/>
					</div>
				</div>
				<div
					className="truncate px-card pb-card pt-4 text-sm font-medium"
					title={game.title}
				>
					{game.title}
					{game.release_year != null && (
						<span className="ml-1.5 font-normal text-muted-foreground">
							{game.release_year}
						</span>
					)}
				</div>
			</Link>
			{/* Hidden: the controls stay visible, since un-hide is the only way back. Otherwise they
			    reveal on hover, focus or a coarse pointer, and pointer-events move with opacity so an
			    invisible button never takes a click. */}
			<div
				className={`absolute right-2 top-2 z-10 flex gap-1 transition-opacity ${
					hidden
						? "opacity-100"
						: "opacity-0 pointer-events-none group-hover:opacity-100 group-hover:pointer-events-auto focus-within:opacity-100 focus-within:pointer-events-auto pointer-coarse:opacity-100 pointer-coarse:pointer-events-auto"
				}`}
			>
				<Button
					variant="secondary"
					size="icon"
					className="size-7 bg-background/80 backdrop-blur"
					aria-label={
						hidden ? m.library_unhide_action() : m.library_hide_action()
					}
					// The native tooltip names the bare icon before a click hides a game.
					title={hidden ? m.library_unhide_action() : m.library_hide_action()}
					aria-pressed={hidden}
					disabled={hiding}
					onClick={onToggleHidden}
				>
					{hidden ? (
						<Eye className="size-3.5" />
					) : (
						<EyeOff className="size-3.5" />
					)}
				</Button>
				{isCustom && (
					<Button
						variant="secondary"
						size="icon"
						className="size-7 bg-background/80 backdrop-blur"
						aria-label={m.library_delete()}
						disabled={deleting}
						onClick={onDelete}
					>
						<Trash2 className="size-3.5 text-destructive" />
					</Button>
				)}
			</div>
		</Card>
	);
};
