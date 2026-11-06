import { RotateCw } from "lucide-react";
import type { FC } from "react";
import type { ActionInfo } from "@/api/gen/model";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Spinner } from "@/components/ui/spinner";
import { m } from "@/paraglide/messages";
import { actionUnavailable } from "@/sections/Host/PowerCard";

/**
 * Settings that only a restart applies, and the restart itself. Absent when nothing waits. Where
 * the host cannot restart itself (started from a terminal), its reason replaces the button.
 */
export const RestartBanner: FC<{
	/** Labels of the settings waiting for a restart. */
	names: string[];
	action?: ActionInfo;
	restarting: boolean;
	onRestart: () => void;
}> = ({ names, action, restarting, onRestart }) => {
	if (names.length === 0 && !restarting) return null;
	return (
		<Card className="border-amber-600/40 dark:border-amber-500/40">
			<CardContent className="flex flex-wrap items-center gap-3">
				<RotateCw
					className="size-4 shrink-0 text-amber-600 dark:text-amber-500"
					aria-hidden
				/>
				<p className="min-w-0 flex-1 text-sm" role="status">
					{restarting
						? m.host_settings_restarting()
						: m.host_settings_restart_banner({ names: names.join(", ") })}
				</p>
				{restarting ? (
					<Spinner className="size-4" />
				) : action?.available ? (
					<Button size="sm" onClick={onRestart}>
						{m.host_power_restart_service()}
					</Button>
				) : (
					action?.unavailable_reason && (
						<p className="w-full text-xs text-muted-foreground">
							{actionUnavailable(action)}
						</p>
					)
				)}
			</CardContent>
		</Card>
	);
};
