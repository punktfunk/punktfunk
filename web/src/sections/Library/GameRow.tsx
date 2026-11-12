import { Link } from "@tanstack/react-router";
import { Eye, EyeOff, Trash2 } from "lucide-react";
import { type FC, useState } from "react";
import { LauncherIcon } from "@/components/launcher-icon";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { m } from "@/paraglide/messages";
import {
	type GameCardProps,
	hideLabel,
	OwnBadge,
	SelectCover,
} from "./GameCard";
import { isOperatorOwned, sourceLabel } from "./helpers";
import { InstallBadge } from "./Install";
import { streamSummary } from "./overrides";

/**
 * One title as a line: a small cover, its name, where it comes from, the host's stream settings,
 * and the same two controls a card has. Many more fit a screen than posters do, which is the
 * point of the row view. In selection mode the row is one checkbox. In a profile's view its hide
 * and its own stream settings carry its mark.
 */
export const GameRow: FC<GameCardProps> = ({
	game,
	onDelete,
	deleting,
	onToggleHidden,
	hiding,
	nameOf,
	download,
	selecting,
	selected = false,
	onSelect,
	who,
}) => {
	const hidden = game.hidden === true;
	const isCustom = isOperatorOwned(game);
	const stream = streamSummary(game.overrides);
	const ownStream = who !== undefined && game.profile?.overrides === true;
	const [failed, setFailed] = useState<Record<string, boolean>>({});
	const src = [game.art.portrait, game.art.header].find(
		(u): u is string => !!u && !failed[u],
	);
	return (
		<li className="relative flex items-center gap-3 px-card py-2">
			<Link
				to="/library/$gameId"
				params={{ gameId: game.id }}
				className="flex min-w-0 flex-1 items-center gap-3 rounded-md focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
			>
				<div
					className={`flex h-14 w-10 shrink-0 items-center justify-center overflow-hidden rounded bg-muted text-muted-foreground${
						hidden ? " opacity-30" : ""
					}`}
				>
					{src ? (
						<img
							src={src}
							alt=""
							loading="lazy"
							decoding="async"
							className="size-full object-cover"
							onError={() => setFailed((prev) => ({ ...prev, [src]: true }))}
						/>
					) : game.icon ? (
						<div className="w-6 [&>svg]:size-full">
							<LauncherIcon icon={game.icon} />
						</div>
					) : null}
				</div>
				<div className="min-w-0 flex-1">
					<p className="truncate text-sm font-medium" title={game.title}>
						{game.title}
						{game.release_year != null && (
							<span className="ml-1.5 font-normal text-muted-foreground">
								{game.release_year}
							</span>
						)}
					</p>
					<div className="mt-1 flex flex-wrap gap-1">
						<Badge variant={isCustom ? "secondary" : "outline"}>
							{sourceLabel(game, nameOf)}
						</Badge>
						{game.platform && game.platform.toUpperCase() !== "PC" && (
							<Badge variant="outline">{game.platform}</Badge>
						)}
						{hidden &&
							(who ? (
								<OwnBadge label={m.profile_view_hidden_badge({ name: who })} />
							) : (
								<Badge variant="secondary">{m.library_hidden_badge()}</Badge>
							))}
						{stream &&
							(ownStream ? (
								<OwnBadge
									label={stream}
									mark={m.profile_view_own({ name: who ?? "" })}
								/>
							) : (
								<Badge variant="secondary">{stream}</Badge>
							))}
						<InstallBadge
							{...(game.install ? { install: game.install } : {})}
							{...(download ? { download } : {})}
						/>
					</div>
				</div>
			</Link>
			<div className={`flex shrink-0 gap-1${selecting ? " invisible" : ""}`}>
				<Button
					variant="ghost"
					size="icon"
					className="size-8"
					aria-label={hideLabel(hidden, who)}
					title={hideLabel(hidden, who)}
					aria-pressed={hidden}
					disabled={hiding}
					onClick={onToggleHidden}
				>
					{hidden ? <Eye className="size-4" /> : <EyeOff className="size-4" />}
				</Button>
				{isCustom && onDelete && (
					<Button
						variant="ghost"
						size="icon"
						className="size-8"
						aria-label={m.library_delete()}
						disabled={deleting}
						onClick={onDelete}
					>
						<Trash2 className="size-4 text-destructive" />
					</Button>
				)}
			</div>
			{selecting && onSelect && (
				<SelectCover
					title={game.title}
					selected={selected}
					onSelect={onSelect}
					className="items-center justify-end px-card"
				/>
			)}
		</li>
	);
};
