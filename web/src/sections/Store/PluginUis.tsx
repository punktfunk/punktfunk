// The plugin UIs installed on this host, as cards on the Plugins page.
//
// This page is where they live now: they used to inject themselves into the sidebar, one entry
// each, which is what made the nav unbounded (design/web-console-overhaul.md §4.1). Pinning is
// the operator's own decision, taken here or in Settings → Navigation.
import { Link } from "@tanstack/react-router";
import { Pin, PinOff } from "lucide-react";
import type { FC } from "react";
import { pluginIcon, uiPlugins, usePlugins } from "@/api/plugins";
import { useSeat } from "@/api/seat";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { pluginPin, togglePin, usePins } from "@/lib/nav";
import { m } from "@/paraglide/messages";

/** Renders nothing when no plugin surfaces a UI — a host with none sees no empty shelf. */
export const PluginUis: FC = () => {
	const { data } = usePlugins();
	const seat = useSeat();
	const [pins, setPins] = usePins();
	const plugins = uiPlugins(data);
	// A plugin's page opens on the box's own runner.
	if (plugins.length === 0 || seat) return null;
	return (
		<div className="space-y-2">
			<h2 className="text-sm font-medium">{m.plugin_uis_title()}</h2>
			<div className="@container">
				<div className="grid grid-cols-1 gap-card @xl:grid-cols-2 @4xl:grid-cols-3">
					{plugins.map((p) => {
						const Icon = pluginIcon(p.ui?.icon);
						const id = pluginPin(p.id);
						const isPinned = pins.includes(id);
						return (
							<Card key={p.id}>
								<CardContent className="flex items-center gap-3">
									<Icon className="size-5 shrink-0 text-muted-foreground" />
									<Link
										to="/plugins/$pluginId/$"
										params={{ pluginId: p.id, _splat: "" }}
										className="min-w-0 flex-1 truncate font-medium hover:underline"
									>
										{p.title}
									</Link>
									<Button
										variant="ghost"
										size="icon"
										aria-pressed={isPinned}
										aria-label={isPinned ? m.nav_unpin() : m.nav_pin()}
										title={isPinned ? m.nav_unpin() : m.nav_pin()}
										onClick={() => setPins(togglePin(pins, id))}
									>
										{isPinned ? (
											<PinOff className="size-4" />
										) : (
											<Pin className="size-4" />
										)}
									</Button>
								</CardContent>
							</Card>
						);
					})}
				</div>
			</div>
		</div>
	);
};
