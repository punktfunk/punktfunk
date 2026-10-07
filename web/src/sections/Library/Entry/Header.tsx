import { Link } from "@tanstack/react-router";
import { ArrowLeft, Eye, EyeOff, Trash2 } from "lucide-react";
import { type FC, type ReactNode, useState } from "react";
import type { OperatorGameEntry } from "@/api/gen/model/operatorGameEntry";
import { LauncherIcon } from "@/components/launcher-icon";
import {
	PasswordConfirmField,
	type PasswordFailure,
} from "@/components/password-confirm";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { fmtAgo, fmtSpan } from "@/lib/format";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";

export interface EntryHeaderProps {
	/** The catalog entry; null while creating. */
	entry: OperatorGameEntry | null;
	/** The draft's title, so the heading follows the typing. */
	title: string;
	/** Store badge text. */
	storeName: string | null;
	/** The source that owns a read-only entry. */
	managedBy: string | null;
	dirty: boolean;
	saving: boolean;
	/** The save carries a command: the console password field shows. */
	gated: boolean;
	password: string;
	onPassword: (value: string) => void;
	/** Why the BFF refused the last save's password, if it did. */
	failure?: PasswordFailure;
	/** Absent on a read-only entry. */
	onSave?: () => void;
	onDelete?: () => void;
	deleting: boolean;
	onToggleHidden?: () => void;
	hiding: boolean;
	error?: string | null;
}

/** The page's inset (the main's padding plus the section's), for a band that runs past it. */
const BLEED =
	"-mx-[calc(1rem+var(--spacing-main))] -mt-[calc(1.5rem+var(--spacing-main))] sm:-mx-[calc(2.5rem+var(--spacing-main))] sm:-mt-[calc(2.5rem+var(--spacing-main))]";

/** The wide banner across the whole top of the page; the poster and title stand on its lower edge. */
const Hero: FC<{ src: string; onFail: () => void; children: ReactNode }> = ({
	src,
	onFail,
	children,
}) => (
	<div className={cn("relative h-52 overflow-hidden @md:h-72", BLEED)}>
		<img
			src={src}
			alt=""
			aria-hidden
			className="size-full object-cover"
			onError={onFail}
		/>
		<div className="absolute inset-0 bg-gradient-to-t from-background via-background/30 to-background/10" />
		<div className="absolute top-4 left-4 sm:top-6 sm:left-10">{children}</div>
	</div>
);

/** Portrait, then whatever art there is, then the brand mark. */
const Poster: FC<{ entry: OperatorGameEntry | null }> = ({ entry }) => {
	const [failed, setFailed] = useState<Record<string, boolean>>({});
	const src = [entry?.art.portrait, entry?.art.header].find(
		(u): u is string => !!u && !failed[u],
	);
	return (
		<div className="relative z-10 flex aspect-[2/3] w-24 shrink-0 items-center justify-center overflow-hidden rounded-lg bg-muted text-muted-foreground shadow-xl ring-1 ring-border @md:w-36">
			{src ? (
				<img
					src={src}
					alt=""
					className="size-full object-cover"
					onError={() => setFailed((prev) => ({ ...prev, [src]: true }))}
				/>
			) : (
				<div className="w-full max-w-12 p-2 [&>svg]:size-full">
					<LauncherIcon icon={entry?.icon} />
				</div>
			)}
		</div>
	);
};

/** What the host has seen of this title: launches, time played, when last. */
const statsLine = (entry: OperatorGameEntry | null): string | null => {
	const s = entry?.stats;
	if (!s || s.launch_count === 0) return null;
	return [
		s.launch_count === 1
			? m.library_entry_launched_once()
			: m.library_entry_launches({ count: s.launch_count }),
		s.play_time_ms >= 60_000 &&
			m.library_entry_play_time({ time: fmtSpan(s.play_time_ms / 1000) }),
		m.library_entry_last_played({ ago: fmtAgo(s.last_played_unix_ms / 1000) }),
	]
		.filter(Boolean)
		.join(" · ");
};

export const EntryHeader: FC<EntryHeaderProps> = ({
	entry,
	title,
	storeName,
	managedBy,
	dirty,
	saving,
	gated,
	password,
	onPassword,
	failure = null,
	onSave,
	onDelete,
	deleting,
	onToggleHidden,
	hiding,
	error,
}) => {
	const [heroFailed, setHeroFailed] = useState(false);
	const hidden = entry?.hidden === true;
	const creating = entry === null;
	const blocked = !dirty || saving || !title.trim() || (gated && !password);
	const hero = !heroFailed ? entry?.art.hero : null;
	// One line of what it is: the source, a console, the year, who made it.
	const meta = [
		managedBy ?? storeName,
		entry?.platform && entry.platform.toUpperCase() !== "PC"
			? entry.platform
			: null,
		entry?.release_year,
		entry?.developer,
	]
		.filter(Boolean)
		.join(" · ");
	const stats = statsLine(entry);
	// On the banner the link gets a backing, so it reads over any picture.
	const back = (
		<Link
			to="/library"
			className={cn(
				"inline-flex w-fit items-center gap-1 text-sm text-muted-foreground hover:text-foreground",
				hero && "rounded-full bg-background/60 px-2.5 py-1 backdrop-blur",
			)}
		>
			<ArrowLeft className="size-3.5" />
			{m.library_title()}
		</Link>
	);
	return (
		<div className="flex flex-col gap-4">
			{!hero && back}
			<div className="@container">
				{hero && (
					<Hero src={hero} onFail={() => setHeroFailed(true)}>
						{back}
					</Hero>
				)}
				<div
					className={cn(
						"flex flex-wrap items-end gap-4",
						hero && "-mt-14 @md:-mt-24",
					)}
				>
					<Poster entry={entry} />
					<div className="min-w-0 flex-1 space-y-1 pb-1">
						<h1 className="break-words text-2xl font-semibold @md:text-3xl">
							{title.trim() ||
								(creating ? m.library_add_title() : entry?.title)}
						</h1>
						{meta && <p className="text-sm text-muted-foreground">{meta}</p>}
						{stats && <p className="text-xs text-muted-foreground">{stats}</p>}
						{hidden && (
							<Badge variant="secondary">{m.library_hidden_badge()}</Badge>
						)}
					</div>
					<div className="flex w-full flex-wrap items-center gap-2 @lg:w-auto @lg:pb-1">
						{onToggleHidden && (
							<Button
								variant="outline"
								size="sm"
								aria-pressed={hidden}
								disabled={hiding}
								onClick={onToggleHidden}
							>
								{hidden ? (
									<Eye className="size-4" />
								) : (
									<EyeOff className="size-4" />
								)}
								{hidden ? m.library_unhide_action() : m.library_hide_action()}
							</Button>
						)}
						{onDelete && (
							<Button
								variant="outline"
								size="sm"
								disabled={deleting}
								onClick={onDelete}
							>
								<Trash2 className="size-4 text-destructive" />
								{m.library_delete()}
							</Button>
						)}
						{onSave && (
							<Button size="sm" disabled={blocked} onClick={onSave}>
								{creating ? m.library_create() : m.library_save()}
							</Button>
						)}
					</div>
				</div>
			</div>
			{managedBy && (
				<p className="text-xs text-muted-foreground">
					{m.library_entry_managed_note({ source: managedBy })}
				</p>
			)}
			{/* Saving a command, or a row that carries prep, runs code as the host user: the
			    console password is asked for exactly when the BFF gate applies. */}
			{onSave && gated && (
				<div className="max-w-sm">
					<PasswordConfirmField
						id="entry-password"
						value={password}
						onChange={onPassword}
						failure={failure}
						help={m.library_field_password_help()}
					/>
				</div>
			)}
			{error && (
				<p
					role="alert"
					className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive"
				>
					{error}
				</p>
			)}
			{/* Phones: the header's Save scrolls away on a long tab, so a dirty page keeps one in
			    reach above the bottom nav. */}
			{onSave && dirty && (
				<div className="fixed inset-x-4 bottom-20 z-30 flex items-center justify-between gap-3 rounded-xl border bg-card/95 px-4 py-3 shadow-lg backdrop-blur sm:hidden">
					<span className="text-sm text-muted-foreground">
						{gated && !password
							? m.library_entry_password_first()
							: m.library_entry_unsaved()}
					</span>
					<Button size="sm" disabled={blocked} onClick={onSave}>
						{creating ? m.library_create() : m.library_save()}
					</Button>
				</div>
			)}
		</div>
	);
};
