import Section from "@unom/ui/section";
import { toast } from "@unom/ui/toast";
import { Globe, Languages, LogOut, PanelLeft, UserRound } from "lucide-react";
import type { FC } from "react";
import { pluginIcon, uiPlugins, usePlugins } from "@/api/plugins";
import { useUiConfig } from "@/api/uiConfig";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
import { Label } from "@/components/ui/label";
import { changeLocale, type Locale, locales, useLocale } from "@/lib/i18n";
import { languageName } from "@/lib/language-name";
import { pluginPin, togglePin, usePins } from "@/lib/nav";
import { m } from "@/paraglide/messages";
import { AppearanceCard } from "./Appearance";

// Settings owns the console's own preferences — the locale, the sidebar pins, and the way
// out. Everything here is per browser (design/web-console-overhaul.md D8); nothing reaches
// the host.
export const SectionSettings: FC = () => {
	const current = useLocale();

	const onLogout = async () => {
		try {
			const res = await fetch("/_auth/logout", { method: "POST" });
			if (!res.ok) throw new Error(`logout failed: ${res.status}`);
			window.location.href = "/login";
		} catch {
			// The logout POST failed, so the session cookie likely survives. Navigating to /login
			// anyway would look logged out while a live session still re-admits on the next gated
			// nav — surface the failure and stay put so the user can retry.
			toast.error(m.settings_logout_failed());
		}
	};

	return (
		<Section maxWidth={false}>
			<div className="flex flex-col gap-card">
				<h1 className="text-2xl font-semibold">{m.settings_title()}</h1>

				<Card className="max-w-lg">
					<CardHeader>
						<CardTitle className="flex items-center gap-2">
							<Languages className="size-4" />
							{m.settings_language()}
						</CardTitle>
					</CardHeader>
					<CardContent className="flex flex-wrap gap-2">
						{locales.map((l: Locale) => (
							<Button
								key={l}
								variant={l === current ? "default" : "outline"}
								size="sm"
								lang={l}
								onClick={() => changeLocale(l)}
							>
								{languageName(l)}
							</Button>
						))}
					</CardContent>
				</Card>

				<AppearanceCard />

				<ReachCard />

				<NavigationCard />

				<Card className="max-w-lg">
					<CardHeader>
						<CardTitle className="flex items-center gap-2">
							<UserRound className="size-4" />
							{m.settings_account()}
						</CardTitle>
					</CardHeader>
					<CardContent>
						<Button variant="outline" onClick={onLogout}>
							<LogOut className="size-4" />
							{m.action_logout()}
						</Button>
					</CardContent>
				</Card>
			</div>
		</Section>
	);
};

/**
 * Who can reach this console, but only when that is more than this machine.
 *
 * The bind is the server's to know (`PUNKTFUNK_UI_BIND`), not the browser's: reaching the page
 * over the LAN proves it, while opening it on the host proves nothing either way. The URL shown
 * is this tab's own, which is an address that demonstrably works.
 */
const ReachCard: FC = () => {
	const { data } = useUiConfig();
	if (!data?.reachableFromNetwork) return null;
	return (
		<Card className="max-w-lg">
			<CardHeader>
				<CardTitle className="flex items-center gap-2">
					<Globe className="size-4" />
					{m.settings_reach_title()}
				</CardTitle>
			</CardHeader>
			<CardContent className="space-y-2">
				<p className="text-sm">
					{m.settings_reach_lan({ url: window.location.origin })}
				</p>
				<p className="text-sm text-muted-foreground">
					{m.settings_reach_change()}
				</p>
			</CardContent>
		</Card>
	);
};

/**
 * Which plugin pages get an entry in the sidebar. Every console page is there already; a
 * plugin's page is not until it is pinned. A plugin appears here as soon as it surfaces a UI —
 * the pin the install toast points at — and with none installed there is no card.
 */
const NavigationCard: FC = () => {
	const [pins, setPins] = usePins();
	const { data } = usePlugins();
	const plugins = uiPlugins(data);
	if (plugins.length === 0) return null;
	const row = (id: string, title: string, Icon: typeof LogOut) => (
		<li key={id} className="flex items-center gap-3 py-1.5">
			<Checkbox
				id={`pin-${id}`}
				checked={pins.includes(id)}
				onCheckedChange={() => setPins(togglePin(pins, id))}
			/>
			<Icon className="size-4 shrink-0 text-muted-foreground" />
			<Label htmlFor={`pin-${id}`} className="font-normal">
				{title}
			</Label>
		</li>
	);
	return (
		<Card className="max-w-lg">
			<CardHeader>
				<CardTitle className="flex items-center gap-2">
					<PanelLeft className="size-4" />
					{m.settings_navigation()}
				</CardTitle>
			</CardHeader>
			<CardContent className="space-y-2">
				<p className="text-sm text-muted-foreground">
					{m.settings_navigation_help()}
				</p>
				<ul>
					{plugins.map((p) =>
						row(pluginPin(p.id), p.title, pluginIcon(p.ui?.icon)),
					)}
				</ul>
			</CardContent>
		</Card>
	);
};
