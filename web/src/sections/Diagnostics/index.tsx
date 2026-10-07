import { Link, Outlet, useRouterState } from "@tanstack/react-router";
import Section from "@unom/ui/section";
import type { FC } from "react";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";

/** The three tools, in the order someone reaches for them when something is off. */
const SEGMENTS = [
	{ to: "/diagnostics/logs", label: () => m.nav_troubleshooting() },
	{ to: "/diagnostics/performance", label: () => m.nav_stats() },
	{ to: "/diagnostics/controllers", label: () => m.nav_controllers() },
] as const;

/**
 * Diagnostics: one destination, one segment per tool. The segment is the URL, so a tab is a link
 * and the router decides which one is on.
 */
export const SectionDiagnostics: FC = () => {
	useLocale();
	const pathname = useRouterState({ select: (s) => s.location.pathname });
	const on = SEGMENTS.find((s) => pathname.startsWith(s.to))?.to;
	return (
		<Section maxWidth={false}>
			<div className="flex flex-col gap-card">
				<h1 className="text-2xl font-semibold">{m.nav_diagnostics()}</h1>
				<Tabs value={on ?? ""} activationMode="manual">
					<TabsList>
						{SEGMENTS.map((s) => (
							<TabsTrigger key={s.to} value={s.to} asChild>
								<Link to={s.to}>{s.label()}</Link>
							</TabsTrigger>
						))}
					</TabsList>
				</Tabs>
				<Outlet />
			</div>
		</Section>
	);
};
