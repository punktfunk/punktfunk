import { toast } from "@unom/ui/toast";
import { Container, Play, Power, PowerOff } from "lucide-react";
import type { FC } from "react";
import {
	type RuntimeStatus,
	useSetRuntime,
	useStoreRuntime,
} from "@/api/store";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { m } from "@/paraglide/messages";

// The plugin/script runner is the service every plugin actually executes inside. Installing a plugin
// while it's switched off silently gets you nothing running, so Browse carries a banner and the
// Installed tab carries the full switch.

/** Small helper both surfaces share: fire the toggle, surface a failure as a toast. */
function useRunnerToggle() {
	const set = useSetRuntime();
	const toggle = (enabled: boolean) => {
		set.mutate(enabled, {
			onError: () => toast.error(m.store_runner_failed()),
		});
	};
	return { toggle, isPending: set.isPending };
}

/**
 * Browse-tab banner: the runner is installed but not running, so nothing an operator installs
 * here would start. A runner the host reports as failing says that instead of "switched off".
 * Renders nothing in every other state (including "not installed" — the Installed tab's card
 * explains that case properly).
 */
export const RunnerBanner: FC = () => {
	const runtime = useStoreRuntime();
	const { toggle, isPending } = useRunnerToggle();
	const s = runtime.data;
	if (!s?.installed || s.running) return null;

	return (
		<div className="flex flex-col gap-3 rounded-lg border border-amber-600/40 bg-amber-500/10 p-4 text-sm text-amber-600 sm:flex-row sm:items-center dark:border-amber-500/40 dark:text-amber-500">
			<PowerOff className="size-5 shrink-0" />
			<p className="flex-1">{s.detail || m.store_runner_banner()}</p>
			<Button size="sm" disabled={isPending} onClick={() => toggle(true)}>
				<Play className="size-4" />
				{m.store_runner_enable()}
			</Button>
		</div>
	);
};

/**
 * Container: the runner switch at the top of the Installed tab. Like the library's source toggles
 * this is a secondary control — it stays out of the way until the query resolves rather than
 * stacking a second error banner on top of the installed list's own.
 */
export const RunnerCardSection: FC = () => {
	const runtime = useStoreRuntime();
	const { toggle, isPending } = useRunnerToggle();
	if (!runtime.data) return null;
	return (
		<RunnerCard status={runtime.data} busy={isPending} onToggle={toggle} />
	);
};

/**
 * The runner card: what the service is, whether it's up, and the one switch that changes it.
 * The switch follows `running`, not `enabled`: an enabled-but-stopped runner must offer a
 * way back up, and enable is what starts it.
 */
export const RunnerCard: FC<{
	status: RuntimeStatus;
	busy: boolean;
	onToggle: (enabled: boolean) => void;
}> = ({ status, busy, onToggle }) => (
	<Card>
		<CardHeader className="pb-3">
			<CardTitle className="flex items-center justify-between gap-3">
				<span className="flex items-center gap-2">
					<Container className="size-4" />
					{m.store_runner_title()}
				</span>
				{!status.installed ? (
					<Badge variant="outline">{m.store_runner_state_missing()}</Badge>
				) : status.running ? (
					<Badge variant="success">{m.store_runner_state_running()}</Badge>
				) : status.enabled ? (
					<Badge variant="outline">{m.store_runner_state_stopped()}</Badge>
				) : (
					<Badge variant="secondary">{m.store_runner_state_disabled()}</Badge>
				)}
			</CardTitle>
		</CardHeader>
		<CardContent className="space-y-3">
			<p className="max-w-prose text-sm text-muted-foreground">
				{status.installed
					? m.store_runner_help()
					: m.store_runner_not_installed()}
			</p>
			<dl className="flex flex-wrap gap-x-8 gap-y-1 text-xs text-muted-foreground">
				<div className="flex gap-2">
					<dt>{m.store_runner_unit()}</dt>
					<dd className="font-mono text-foreground">{status.unit}</dd>
				</div>
				{status.principal && (
					<div className="flex gap-2">
						<dt>{m.store_runner_principal()}</dt>
						<dd className="font-mono text-foreground">{status.principal}</dd>
					</div>
				)}
			</dl>
			{status.detail && (
				<p className="text-xs text-muted-foreground">{status.detail}</p>
			)}
			{status.installed && (
				<Button
					size="sm"
					variant={status.running ? "outline" : "default"}
					disabled={busy}
					onClick={() => onToggle(!status.running)}
				>
					{status.running ? (
						<PowerOff className="size-4" />
					) : (
						<Power className="size-4" />
					)}
					{status.running ? m.store_runner_disable() : m.store_runner_enable()}
				</Button>
			)}
		</CardContent>
	</Card>
);
