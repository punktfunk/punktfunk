import { Link } from "@tanstack/react-router";
import { Eye, EyeOff, Trash2 } from "lucide-react";
import { type FC, useState } from "react";
import type { Download } from "@/api/gen/model/download";
import type { OperatorGameEntry } from "@/api/gen/model/operatorGameEntry";
import { LauncherIcon } from "@/components/launcher-icon";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";
import { isOperatorOwned } from "./helpers";
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
 * A poster that opens the entry's page. The cover prefers the 2:3 portrait; on a load error it
 * falls back to the wide header, then to the title. Only what a cover cannot say sits on it: a
 * console's platform, Hidden, and the install mark; the source is the entry page's. Hide and
 * delete appear on hover, on a pointer that can hover; a phone manages a title on its page.
 */
export const GameCard: FC<GameCardProps> = ({
	game,
	onDelete,
	deleting,
	onToggleHidden,
	hiding,
	download,
}) => {
	// Every store can be hidden: the titles most worth hiding are the ones the operator cannot
	// edit. The host keys the setting by entry id and never needs to own the entry.
	const hidden = game.hidden === true;
	// Only the operator's own entries delete; the host refuses a provider-synced one (409).
	const isCustom = isOperatorOwned(game);
	const [failed, setFailed] = useState<Record<string, boolean>>({});
	const src = [game.art.portrait, game.art.header].find(
		(u): u is string => !!u && !failed[u],
	);
	const emulated = game.platform && game.platform.toUpperCase() !== "PC";
	return (
		<Card className="group relative overflow-hidden">
			<Link
				to="/library/$gameId"
				params={{ gameId: game.id }}
				className="block rounded-[inherit] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
			>
				<div className="relative aspect-[2/3] bg-muted">
					{/* Only the artwork dims: the badges over it say why, at full contrast. */}
					{src ? (
						<img
							src={src}
							alt={game.title}
							loading="lazy"
							decoding="async"
							className={cn("size-full object-cover", hidden && "opacity-30")}
							onError={() => setFailed((prev) => ({ ...prev, [src]: true }))}
						/>
					) : game.icon ? (
						<div
							className={cn(
								"flex size-full items-center justify-center p-6 text-muted-foreground",
								hidden && "opacity-30",
							)}
						>
							<div className="w-full max-w-16 [&>svg]:size-full">
								<LauncherIcon icon={game.icon} />
							</div>
						</div>
					) : (
						<div
							className={cn(
								"flex size-full items-center justify-center p-3 text-center text-sm font-medium text-muted-foreground",
								hidden && "opacity-30",
							)}
						>
							{game.title}
						</div>
					)}
					<div className="absolute top-1.5 left-1.5 flex flex-wrap gap-1">
						{emulated && (
							<Badge
								variant="outline"
								className="bg-background/80 px-1.5 text-[10px] backdrop-blur"
							>
								{game.platform}
							</Badge>
						)}
						{hidden && (
							<Badge
								variant="secondary"
								className="bg-background/90 px-1.5 text-[10px] backdrop-blur"
							>
								{m.library_hidden_badge()}
							</Badge>
						)}
					</div>
					<div className="absolute bottom-1.5 left-1.5">
						<InstallBadge
							{...(game.install ? { install: game.install } : {})}
							{...(download ? { download } : {})}
						/>
					</div>
				</div>
				<div className="px-2.5 py-2">
					<p
						className="line-clamp-2 text-sm leading-snug font-medium"
						title={game.title}
					>
						{game.title}
					</p>
					{game.release_year != null && (
						<p className="text-xs text-muted-foreground">{game.release_year}</p>
					)}
				</div>
			</Link>
			{/* Hidden: the controls stay, since un-hide is the way back. Otherwise they appear on
			    hover or focus, and pointer-events follow opacity so an unseen button takes no click. */}
			<div
				className={cn(
					"absolute top-1.5 right-1.5 z-10 hidden gap-1 transition-opacity sm:flex",
					hidden
						? "opacity-100"
						: "pointer-events-none opacity-0 group-hover:pointer-events-auto group-hover:opacity-100 focus-within:pointer-events-auto focus-within:opacity-100",
				)}
			>
				<Button
					variant="secondary"
					size="icon"
					className="size-7 bg-background/80 backdrop-blur"
					aria-label={
						hidden ? m.library_unhide_action() : m.library_hide_action()
					}
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
						title={m.library_delete()}
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
