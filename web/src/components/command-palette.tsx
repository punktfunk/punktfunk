import { useNavigate } from "@tanstack/react-router";
import { defaultFilter } from "cmdk";
import { Gamepad2, Plus, Search, Settings2, Smartphone } from "lucide-react";
import { type FC, useEffect, useState } from "react";
import { useGetHostSettings } from "@/api/gen/host/host";
import { useGetLibraryPage } from "@/api/gen/library/library";
import { useListNativeClients } from "@/api/gen/native/native";
import { pluginIcon, uiPlugins, usePlugins } from "@/api/plugins";
import { Button } from "@/components/ui/button";
import {
	CommandDialog,
	CommandEmpty,
	CommandGroup,
	CommandInput,
	CommandItem,
	CommandList,
	CommandShortcut,
} from "@/components/ui/command";
import { NAV } from "@/lib/nav";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";
import { labelOf } from "@/sections/HostSettings/controls";

/** Games come from the host's own search, a few at a time: a library runs to thousands. */
const GAMES = 8;

/** Match on what a row says, never its `value`: that is an id, and `device …` would match "dev". */
const byKeywords = (_value: string, search: string, keywords?: string[]) =>
	defaultFilter(keywords?.join(" ") ?? "", search);

/** ⌘ on a Mac, Ctrl elsewhere. Read after mount, so the server's render and the first paint agree. */
function useShortcutLabel(): string {
	const [mac, setMac] = useState(false);
	useEffect(() => setMac(/Mac|iPhone|iPad/.test(navigator.platform)), []);
	return mac ? "⌘K" : "Ctrl K";
}

/**
 * The console's search: every page, a few actions, the plugin pages, then the host's games,
 * paired devices and settings, each a jump to where it lives. Opens on ⌘K / Ctrl+K or `open`.
 * Its lists load only while it is open.
 */
export const CommandPalette: FC<{
	open: boolean;
	onOpenChange: (open: boolean) => void;
}> = ({ open, onOpenChange }) => {
	const navigate = useNavigate();
	const [search, setSearch] = useState("");
	const q = search.trim();

	useEffect(() => {
		const onKey = (e: KeyboardEvent) => {
			if (e.key.toLowerCase() === "k" && (e.metaKey || e.ctrlKey)) {
				e.preventDefault();
				onOpenChange(!open);
			}
		};
		window.addEventListener("keydown", onKey);
		return () => window.removeEventListener("keydown", onKey);
	}, [open, onOpenChange]);
	useEffect(() => {
		if (!open) setSearch("");
	}, [open]);

	const plugins = usePlugins();
	const devices = useListNativeClients({
		query: { enabled: open, retry: false },
	});
	const settings = useGetHostSettings({
		query: { enabled: open, retry: false },
	});
	const games = useGetLibraryPage(
		{ q, limit: GAMES },
		{ query: { enabled: open && q.length >= 2, retry: false } },
	);

	const go = (to: () => void) => {
		onOpenChange(false);
		to();
	};

	return (
		<CommandDialog
			open={open}
			onOpenChange={onOpenChange}
			title={m.palette_title()}
			description={m.palette_description()}
			commandProps={{ filter: byKeywords }}
		>
			<CommandInput
				placeholder={m.palette_placeholder()}
				value={search}
				onValueChange={setSearch}
			/>
			<CommandList>
				<CommandEmpty>{m.palette_empty()}</CommandEmpty>
				<CommandGroup heading={m.palette_group_pages()}>
					{NAV.map((n) => (
						<CommandItem
							key={n.to}
							value={`page ${n.to}`}
							keywords={[n.label(), n.hint()]}
							onSelect={() => go(() => navigate({ to: n.to }))}
						>
							<n.icon />
							{n.label()}
						</CommandItem>
					))}
					{uiPlugins(plugins.data).map((p) => {
						const Icon = pluginIcon(p.ui?.icon);
						return (
							<CommandItem
								key={p.id}
								value={`plugin ${p.id}`}
								keywords={[p.title]}
								onSelect={() =>
									go(() =>
										navigate({
											to: "/plugins/$pluginId/$",
											params: { pluginId: p.id, _splat: "" },
										}),
									)
								}
							>
								<Icon />
								{p.title}
							</CommandItem>
						);
					})}
				</CommandGroup>
				<CommandGroup heading={m.palette_group_actions()}>
					<CommandItem
						value="action pair"
						keywords={[m.pairing_native_title()]}
						onSelect={() =>
							go(() => navigate({ to: "/devices", search: { pair: true } }))
						}
					>
						<Plus />
						{m.pairing_native_title()}
					</CommandItem>
					<CommandItem
						value="action add-game"
						keywords={[m.library_add_button()]}
						onSelect={() =>
							go(() =>
								navigate({
									to: "/library/$gameId",
									params: { gameId: "new" },
								}),
							)
						}
					>
						<Plus />
						{m.library_add_button()}
					</CommandItem>
				</CommandGroup>
				{(games.data?.items.length ?? 0) > 0 && (
					<CommandGroup heading={m.palette_group_games()}>
						{games.data?.items.map((g) => (
							<CommandItem
								key={g.id}
								value={`game ${g.id}`}
								// The host matched it already, on more than the title.
								keywords={[g.title, q]}
								onSelect={() =>
									go(() =>
										navigate({
											to: "/library/$gameId",
											params: { gameId: g.id },
										}),
									)
								}
							>
								<Gamepad2 />
								<span className="truncate">{g.title}</span>
							</CommandItem>
						))}
					</CommandGroup>
				)}
				{(devices.data?.length ?? 0) > 0 && (
					<CommandGroup heading={m.palette_group_devices()}>
						{devices.data?.map((d) => (
							<CommandItem
								key={d.fingerprint}
								value={`device ${d.fingerprint}`}
								keywords={[d.name]}
								onSelect={() => go(() => navigate({ to: "/devices" }))}
							>
								<Smartphone />
								<span className="truncate">{d.name}</span>
							</CommandItem>
						))}
					</CommandGroup>
				)}
				{/* Settings only once something is typed: forty rows would bury the pages. */}
				{q && (settings.data?.settings.length ?? 0) > 0 && (
					<CommandGroup heading={m.palette_group_settings()}>
						{settings.data?.settings.map((row) => (
							<CommandItem
								key={row.id}
								value={`setting ${row.id}`}
								keywords={[labelOf(row), row.env]}
								onSelect={() =>
									go(() =>
										navigate({ to: "/host", search: { q: labelOf(row) } }),
									)
								}
							>
								<Settings2 />
								<span className="truncate">{labelOf(row)}</span>
							</CommandItem>
						))}
					</CommandGroup>
				)}
			</CommandList>
		</CommandDialog>
	);
};

/** The way into the palette for a pointer: a search field look-alike in the sidebar, an icon on
 * the phone's top bar. */
export const PaletteButton: FC<{ onOpen: () => void; compact?: boolean }> = ({
	onOpen,
	compact,
}) => {
	const shortcut = useShortcutLabel();
	return compact ? (
		<Button
			variant="ghost"
			size="icon"
			aria-label={m.palette_title()}
			title={m.palette_title()}
			onClick={onOpen}
		>
			<Search className="size-4" />
		</Button>
	) : (
		<button
			type="button"
			onClick={onOpen}
			title={m.palette_title()}
			aria-label={m.palette_title()}
			className={cn(
				"flex w-full items-center justify-center gap-2 rounded-md border border-input px-3 py-2 text-sm text-muted-foreground transition-colors hover:bg-primary/15 hover:text-foreground lg:justify-start",
			)}
		>
			<Search className="size-4 shrink-0" />
			<span className="hidden flex-1 text-left lg:inline">
				{m.palette_search()}
			</span>
			<CommandShortcut className="ml-0 hidden lg:inline">
				{shortcut}
			</CommandShortcut>
		</button>
	);
};
