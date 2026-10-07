import { Link, useRouterState } from "@tanstack/react-router";
import { MoreHorizontal } from "lucide-react";
import { AnimatePresence, motion, stagger, type Variants } from "motion/react";
import { type ReactNode, useState } from "react";
import { useHostEvents } from "@/api/events";
import { pluginIcon, uiPlugins, usePlugins } from "@/api/plugins";
import { BrandMark } from "@/components/brand-mark";
import { ROW, Stagger, staggerProps } from "@/components/stagger";
import { Wordmark } from "@/components/wordmark";
import { changeLocale, type Locale, locales, useLocale } from "@/lib/i18n";
import {
	MANAGE,
	type NavEntry,
	PHONE_BAR,
	PHONE_SHEET,
	PRIMARY,
	pluginPin,
	resolvePins,
	usePins,
} from "@/lib/nav";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";
import { useNewPluginToast } from "./plugin-toast";

const MLink = motion(Link);

// Centred in the collapsed rail, where the label is hidden; left-aligned beside it from lg.
const ITEM =
	"group relative flex items-center justify-center gap-3 rounded-md px-3 py-2 text-sm text-muted-foreground transition-colors hover:text-foreground lg:justify-start";
const ACTIVE = { className: "bg-primary/15 text-foreground font-medium" };
const RISE = { from: { opacity: 0, x: -20 }, enter: { opacity: 1, x: 0 } };

/** The phone bar slides up on load and its tabs rise in turn behind it, as the sidebar's items do. */
const BAR: Variants = {
	from: { y: "100%" },
	enter: {
		y: 0,
		transition: {
			type: "spring",
			bounce: 0,
			duration: 0.45,
			delayChildren: stagger(0.05, { startDelay: 0.1 }),
		},
	},
};
const TAB = { from: { opacity: 0, y: 12 }, enter: { opacity: 1, y: 0 } };

/** The More sheet rises over the bar with its rows following; `exit` plays it back out. */
const SHEET: Variants = {
	from: { opacity: 0, y: 24 },
	enter: {
		opacity: 1,
		y: 0,
		transition: {
			type: "spring",
			bounce: 0,
			duration: 0.35,
			delayChildren: stagger(0.03, { startDelay: 0.05 }),
		},
	},
	exit: { opacity: 0, y: 16, transition: { duration: 0.15 } },
};

/** A press answers at once. Objects, never labels: a label would stop the item inheriting its entrance. */
const PRESS = { scale: 0.9 };
const ROW_PRESS = { scale: 0.98 };

export function AppShell({ children }: { children: ReactNode }) {
	// Read the locale so the whole shell re-renders on a language switch.
	useLocale();
	// One subscription to the host's event stream for the whole console — it invalidates the queries
	// each event affects, so pages update on the transition instead of on their own timer. The
	// polling intervals stay as a floor in case the stream is unavailable.
	useHostEvents();
	// Plugin UIs no longer inject themselves into the sidebar, so a fresh install has to say
	// where it went (design §4.1).
	useNewPluginToast();
	return (
		<div className="flex min-h-screen">
			{/* Desktop sidebar (≥ sm). Sticky at viewport height: the page (body) scrolls with
			    long content, but the sidebar stays pinned — the explicit h-dvh stops the flex
			    stretch that would otherwise grow it (and push the language switcher) below the
			    fold. overflow-y-auto lets the nav itself scroll on very short viewports.

			    Tablet widths (sm–lg) collapse it to icons: the console used to drop straight from
			    a 240 px sidebar to the phone layout, so a tablet either lost a third of its width
			    to labels or lost the sidebar entirely. Labels ride `title` there. */}
			<aside className="sticky top-0 hidden h-dvh w-16 shrink-0 flex-col overflow-y-auto border-r bg-card/40 p-2 sm:flex lg:w-60 lg:p-4">
				<Link
					to="/"
					aria-label="Punktfunk"
					className="mb-7 flex items-center justify-center gap-2 px-2 pt-1 lg:justify-start"
				>
					<BrandMark className="size-7 shrink-0 drop-shadow-[0_2px_12px_rgba(108,91,243,0.45)]" />
					<Wordmark className="hidden h-4 lg:block" />
				</Link>
				<Sidebar />
				<div className="mt-auto hidden pt-4 lg:block">
					<LanguageSwitcher />
				</div>
			</aside>

			<div className="flex flex-1 flex-col overflow-x-hidden">
				{/* Mobile top bar (< sm): brand only. The language switch is a set-once control,
				    so on a phone it lives in Settings rather than in every screen's chrome. */}
				<header className="flex items-center gap-2 border-b bg-card/40 px-4 py-3 sm:hidden">
					<BrandMark className="size-6" />
					<Wordmark className="h-3.5" />
				</header>

				<main className="flex-1">
					{/* Mobile: a 16px side gutter (matching the top bar) so content isn't overly narrow;
					    pb-24 leaves room for the fixed bottom nav. Roomier padding from sm up. */}
					<div className="mx-auto max-w-[1700px] px-4 py-6 pb-24 sm:p-10 sm:pb-10">
						{children}
					</div>
				</main>
			</div>

			<MobileNav />
		</div>
	);
}

/** The sidebar's three groups: the primary pages, Manage, then the plugin pages the operator pinned. */
function Sidebar() {
	const [pins] = usePins();
	const { data } = usePlugins();
	const plugins = uiPlugins(data);
	const pinned = resolvePins(pins, plugins);
	return (
		<motion.nav
			animate="enter"
			initial="from"
			{...staggerProps()}
			className="flex flex-col gap-1"
		>
			{PRIMARY.map((n) => (
				<SidebarLink key={n.to} entry={n} />
			))}
			<GroupLabel>{m.nav_group_manage()}</GroupLabel>
			{MANAGE.map((n) => (
				<SidebarLink key={n.to} entry={n} />
			))}
			{/* Its own group, driving itself: the pins come from the plugins query and land after
			    the nav above them has finished arriving, so they get their own cadence on arrival
			    rather than a slot in a cascade that is over. */}
			{pinned.length > 0 && (
				<Stagger root className="flex flex-col gap-1">
					<GroupLabel>{m.nav_group_pinned()}</GroupLabel>
					{pinned.map((p) => (
						<PluginLink
							key={`pin-${p.id}`}
							id={p.id}
							title={p.title}
							icon={p.ui?.icon}
						/>
					))}
				</Stagger>
			)}
		</motion.nav>
	);
}

/** A group heading. Invisible at icon width, where a divider does the same job in 1 px. */
function GroupLabel({ children }: { children: ReactNode }) {
	return (
		<motion.p
			variants={{ from: { opacity: 0 }, enter: { opacity: 1 } }}
			className="mt-6 border-t pt-4 px-3 pb-1 text-xs font-medium uppercase tracking-wide text-muted-foreground/70 lg:border-0 lg:pt-0"
		>
			<span className="hidden lg:inline">{children}</span>
		</motion.p>
	);
}

function SidebarLink({ entry }: { entry: NavEntry }) {
	const { to, icon: Icon, label, exact } = entry;
	return (
		<MLink
			variants={RISE}
			whileHover={{ scale: 1.02 }}
			whileTap={{ scale: 0.98 }}
			to={to}
			title={label()}
			activeOptions={{ exact: exact === true }}
			className={ITEM}
			activeProps={ACTIVE}
		>
			{/* Hover brightens: a brand-tinted wash layered OVER whatever the link's background
			    is (transparent or the active tint), so the item gets lighter on hover —
			    including the active one. */}
			<span
				aria-hidden
				className="pointer-events-none absolute inset-0 rounded-md bg-primary/0 transition-colors duration-200 group-hover:bg-primary/15"
			/>
			<Icon className="relative size-4 shrink-0" />
			<span className="relative hidden lg:inline">{label()}</span>
		</MLink>
	);
}

/** A pinned plugin's own UI. Its own component because `$pluginId` needs typed `params`. */
function PluginLink({
	id,
	title,
	icon,
}: {
	id: string;
	title: string;
	icon?: string | null;
}) {
	const Icon = pluginIcon(icon);
	return (
		// The motion wrapper is a DIV around the link, not `motion(Link)`: wrapping Link erases
		// TanStack's typed `params`, and this entry needs `$pluginId`.
		<motion.div
			variants={RISE}
			whileHover={{ scale: 1.02 }}
			whileTap={{ scale: 0.98 }}
		>
			<Link
				to="/plugins/$pluginId/$"
				params={{ pluginId: id, _splat: "" }}
				title={title}
				className={ITEM}
				activeProps={ACTIVE}
			>
				<span
					aria-hidden
					className="pointer-events-none absolute inset-0 rounded-md bg-primary/0 transition-colors duration-200 group-hover:bg-primary/15"
				/>
				<Icon className="relative size-4 shrink-0" />
				<span className="relative hidden truncate lg:inline">{title}</span>
			</Link>
		</motion.div>
	);
}

/**
 * Mobile bottom navigation (< sm): four primary pages, plus "More".
 *
 * "More" opens a LIST — icon, label, one line of what the page is for — grouped Manage /
 * Pinned / Plugins. It was a 4-column icon grid with 10 px labels, which is unreadable and
 * says nothing about what a page does.
 *
 * The bar slides up on load with its tabs rising in turn. The sheet rises over it with its rows
 * following, and every tab and row answers a press. A pill behind the icon marks the page.
 */
function MobileNav() {
	const [moreOpen, setMoreOpen] = useState(false);
	const pathname = useRouterState({ select: (s) => s.location.pathname });
	const [pins] = usePins();
	const { data } = usePlugins();
	const plugins = uiPlugins(data);
	// 400 px holds four legible tabs and More. The other primary pages open the sheet above
	// Manage, in their own unlabelled group — a primary destination filed under "Manage" is a lie
	// about what it is.
	const overflow = [...PHONE_SHEET, ...MANAGE];
	const pinnedPlugins = plugins.filter((p) => pins.includes(pluginPin(p.id)));
	const rest = plugins.filter((p) => !pins.includes(pluginPin(p.id)));
	// Highlight "More" when the current route lives in the sheet — plugins included.
	const overflowActive =
		pathname.startsWith("/plugins/") ||
		overflow.some((n) => pathname === n.to || pathname.startsWith(`${n.to}/`));
	const tab =
		"flex flex-1 flex-col items-center justify-center gap-1 px-0.5 py-2 text-muted-foreground transition-colors";
	const lbl = "w-full truncate text-center text-xs leading-tight";
	const tabOn = "font-medium text-foreground";
	const close = () => setMoreOpen(false);
	// Which tab marks the page: a bar tab by its route, else More for anything in the sheet.
	const on = (to: string, exact?: boolean) =>
		pathname === to || (!exact && pathname.startsWith(`${to}/`));
	const activeTo =
		PHONE_BAR.find((n) => on(n.to, n.exact))?.to ??
		(overflowActive ? "more" : null);
	return (
		<>
			{/* Tap-outside backdrop, under the bar (z-50) but over the page. */}
			<AnimatePresence>
				{moreOpen && (
					<motion.button
						type="button"
						aria-label={m.nav_close_menu()}
						className="fixed inset-0 z-40 bg-black/40 sm:hidden"
						onClick={close}
						initial={{ opacity: 0 }}
						animate={{ opacity: 1 }}
						exit={{ opacity: 0 }}
					/>
				)}
			</AnimatePresence>
			<motion.nav
				initial="from"
				animate="enter"
				variants={BAR}
				className="fixed inset-x-0 bottom-0 z-50 sm:hidden"
				style={{ paddingBottom: "env(safe-area-inset-bottom)" }}
			>
				{/* The "More" sheet sits directly above the bar (bottom-full of the fixed nav).
				    Capped at 70vh so a host with several plugins still shows the bar under it. */}
				<AnimatePresence>
					{moreOpen && (
						<motion.div
							initial="from"
							animate="enter"
							exit="exit"
							variants={SHEET}
							className="absolute inset-x-0 bottom-full max-h-[70vh] overflow-y-auto border-t bg-card/95 backdrop-blur"
						>
							{PHONE_SHEET.length > 0 && (
								<MoreGroup>
									{PHONE_SHEET.map((n) => (
										<MoreRow key={n.to} entry={n} onNavigate={close} />
									))}
								</MoreGroup>
							)}
							<MoreGroup label={m.nav_group_manage()}>
								{MANAGE.map((n) => (
									<MoreRow key={n.to} entry={n} onNavigate={close} />
								))}
							</MoreGroup>
							{pinnedPlugins.length > 0 && (
								<MoreGroup label={m.nav_group_pinned()}>
									{pinnedPlugins.map((p) => (
										<MorePluginRow key={p.id} plugin={p} onNavigate={close} />
									))}
								</MoreGroup>
							)}
							{rest.length > 0 && (
								<MoreGroup label={m.nav_plugins()}>
									{rest.map((p) => (
										<MorePluginRow key={p.id} plugin={p} onNavigate={close} />
									))}
								</MoreGroup>
							)}
						</motion.div>
					)}
				</AnimatePresence>
				<div className="flex border-t bg-card/95 backdrop-blur">
					{PHONE_BAR.map(({ to, icon: Icon, label, exact }) => (
						<MLink
							key={to}
							to={to}
							onClick={close}
							variants={TAB}
							whileTap={PRESS}
							activeOptions={{ exact: exact === true }}
							className={cn(tab, activeTo === to && tabOn)}
						>
							<TabIcon active={activeTo === to}>
								<Icon className="size-5 shrink-0" />
							</TabIcon>
							<span className={lbl}>{label()}</span>
						</MLink>
					))}
					<motion.button
						type="button"
						variants={TAB}
						whileTap={PRESS}
						onClick={() => setMoreOpen((o) => !o)}
						aria-expanded={moreOpen}
						className={cn(tab, (moreOpen || overflowActive) && tabOn)}
					>
						<TabIcon active={activeTo === "more"}>
							<MoreHorizontal className="size-5 shrink-0" />
						</TabIcon>
						<span className={lbl}>{m.nav_more()}</span>
					</motion.button>
				</div>
			</motion.nav>
		</>
	);
}

/**
 * A tab's icon, over the pill that marks the page you are on. The pill is one shared `layoutId`,
 * so switching tabs slides it across instead of blinking it out and back in.
 */
function TabIcon({
	active,
	children,
}: {
	active: boolean;
	children: ReactNode;
}) {
	return (
		<span className="relative flex h-7 w-12 items-center justify-center">
			{active && (
				<motion.span
					layoutId="tab-pill"
					className="absolute inset-0 rounded-full bg-primary/15"
					transition={{ type: "spring", bounce: 0.2, duration: 0.4 }}
				/>
			)}
			<span className="relative">{children}</span>
		</span>
	);
}

function MoreGroup({
	label,
	children,
}: {
	label?: string;
	children: ReactNode;
}) {
	return (
		<div className="border-b last:border-0">
			{label && (
				<p className="px-4 pt-3 pb-1 text-xs font-medium uppercase tracking-wide text-muted-foreground/70">
					{label}
				</p>
			)}
			<ul className={label ? undefined : "pt-2"}>{children}</ul>
		</div>
	);
}

/**
 * A 44 px-tall row: icon, label, and the one line that says what the page is for. The router's
 * `data-status` marks the page you are on; an appended active class lost to the muted colour.
 */
const MORE_ROW =
	"flex items-center gap-3 px-4 py-2.5 text-sm text-muted-foreground transition-colors data-[status=active]:bg-primary/15 data-[status=active]:text-foreground";

function MoreRow({
	entry,
	onNavigate,
}: {
	entry: NavEntry;
	onNavigate: () => void;
}) {
	const { to, icon: Icon, label, hint, exact } = entry;
	return (
		<motion.li variants={ROW} whileTap={ROW_PRESS}>
			<Link
				to={to}
				onClick={onNavigate}
				activeOptions={{ exact: exact === true }}
				className={MORE_ROW}
			>
				<Icon className="size-5 shrink-0" />
				<span className="min-w-0">
					<span className="block font-medium text-foreground">{label()}</span>
					<span className="block truncate text-xs">{hint()}</span>
				</span>
			</Link>
		</motion.li>
	);
}

function MorePluginRow({
	plugin,
	onNavigate,
}: {
	plugin: { id: string; title: string; ui?: { icon?: string | null } | null };
	onNavigate: () => void;
}) {
	const Icon = pluginIcon(plugin.ui?.icon);
	return (
		<motion.li variants={ROW} whileTap={ROW_PRESS}>
			<Link
				to="/plugins/$pluginId/$"
				params={{ pluginId: plugin.id, _splat: "" }}
				onClick={onNavigate}
				className={MORE_ROW}
			>
				<Icon className="size-5 shrink-0" />
				<span className="block min-w-0 truncate font-medium text-foreground">
					{plugin.title}
				</span>
			</Link>
		</motion.li>
	);
}

export function LanguageSwitcher() {
	const current = useLocale();
	return (
		// biome-ignore lint/a11y/useSemanticElements: an aria-labelled role="group" is the right pattern for this small control cluster — no single semantic element fits.
		<div className="flex gap-1" role="group" aria-label={m.settings_language()}>
			{locales.map((l: Locale) => (
				<button
					key={l}
					type="button"
					onClick={() => changeLocale(l)}
					className={cn(
						"rounded px-2 py-1 text-xs uppercase transition-colors",
						l === current
							? "bg-primary/20 text-foreground font-medium"
							: "text-muted-foreground hover:text-foreground",
					)}
				>
					{l}
				</button>
			))}
		</div>
	);
}
