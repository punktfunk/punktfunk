import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import {
	CheckCircle2,
	ChevronDown,
	ChevronRight,
	Copy,
	ListChecks,
	RefreshCw,
} from "lucide-react";
import { motion } from "motion/react";
import { type FC, useState } from "react";
import { ApiError } from "@/api/fetcher";
import {
	getGetDiagnosticsQueryKey,
	useGetDiagnostics,
	useRefreshDiagnostics,
} from "@/api/gen/diagnostics/diagnostics";
import type { HostCheck } from "@/api/gen/model/hostCheck";
import { ROW, ROW_GAP, staggerProps } from "@/components/stagger";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardTitle } from "@/components/ui/card";
import {
	checkTitle,
	needsAttention,
	statusLabel,
	statusVariant,
	worstFirst,
} from "@/lib/diagnostics";
import { apiErrorMessage } from "@/lib/errors";
import { m } from "@/paraglide/messages";

/**
 * The troubleshooting page's checks list: everything the host knows about its own health.
 *
 * Unlike the dashboard strip, this shows the `ok` rows too — "what is working" is the reassurance a
 * dashboard deliberately omits, and it is most of the value when someone is hunting a problem that
 * turns out to be elsewhere. `inapplicable` rows hide behind a toggle: they must not nag, but the
 * page still has to be able to answer "why isn't this check relevant on my machine?".
 */

export const ChecksSection: FC = () => {
	const qc = useQueryClient();
	const diagnostics = useGetDiagnostics({
		query: { staleTime: 5 * 60_000, retry: false },
	});
	const refresh = useRefreshDiagnostics();

	const rerun = () =>
		refresh.mutate(undefined, {
			// The response IS the fresh report, so seed the shared cache entry with it instead of
			// invalidating and making the host run every probe a second time.
			onSuccess: (data) => qc.setQueryData(getGetDiagnosticsQueryKey(), data),
			onError: (e) => toast.error(apiErrorMessage(e) ?? m.diag_rerun_failed()),
		});

	// A host that predates this console has no such route. Pairing N with N−1 is supported, so this
	// renders as a plain note rather than an error — nothing is broken, this host just can't answer.
	const unsupported =
		diagnostics.error instanceof ApiError && diagnostics.error.status === 404;

	return (
		<ChecksCard
			checks={diagnostics.data?.checks ?? []}
			unsupported={unsupported}
			isLoading={diagnostics.isLoading}
			error={unsupported ? undefined : diagnostics.error}
			onRerun={rerun}
			isRerunning={refresh.isPending}
		/>
	);
};

export const ChecksCard: FC<{
	checks: HostCheck[];
	/** The host has no diagnostics route (an older host paired with this console). */
	unsupported?: boolean;
	isLoading?: boolean;
	error?: unknown;
	onRerun: () => void;
	isRerunning?: boolean;
}> = ({ checks, unsupported, isLoading, error, onRerun, isRerunning }) => {
	const [showInapplicable, setShowInapplicable] = useState(false);

	const applicable = worstFirst(
		checks.filter((c) => c.status !== "inapplicable"),
	);
	const inapplicable = worstFirst(
		checks.filter((c) => c.status === "inapplicable"),
	);
	const problems = applicable.filter(needsAttention).length;

	return (
		<Card>
			<CardContent className="flex flex-col gap-3">
				<div className="flex flex-wrap items-center justify-between gap-2">
					<CardTitle>
						<h2 className="flex items-center gap-2">
							<ListChecks className="size-4" />
							{m.diag_checks_title()}
						</h2>
					</CardTitle>
					<Button
						variant="outline"
						size="sm"
						onClick={onRerun}
						disabled={isRerunning || unsupported}
					>
						<RefreshCw className="size-3.5" />
						{isRerunning ? m.diag_rerunning() : m.diag_rerun()}
					</Button>
				</div>

				{unsupported ? (
					<p className="text-sm text-muted-foreground">
						{m.diag_unavailable()}
					</p>
				) : error ? (
					<p className="text-sm text-destructive">
						{apiErrorMessage(error) ?? m.diag_rerun_failed()}
					</p>
				) : isLoading ? (
					<p className="text-sm text-muted-foreground">{m.diag_loading()}</p>
				) : (
					<>
						{problems === 0 && applicable.length > 0 && (
							<p className="flex items-center gap-2 text-sm text-muted-foreground">
								<CheckCircle2 className="size-4 text-[var(--success)]" />
								{m.diag_all_ok()}
							</p>
						)}
						<motion.ul
							{...staggerProps(ROW_GAP)}
							className="flex flex-col gap-2"
						>
							{applicable.map((check) => (
								<CheckRow key={check.id} check={check} />
							))}
						</motion.ul>
						{inapplicable.length > 0 && (
							<div className="flex flex-col gap-2">
								<button
									type="button"
									className="flex items-center gap-1 self-start text-xs text-muted-foreground hover:text-foreground"
									aria-expanded={showInapplicable}
									onClick={() => setShowInapplicable((v) => !v)}
								>
									{showInapplicable ? (
										<ChevronDown className="size-3" />
									) : (
										<ChevronRight className="size-3" />
									)}
									{showInapplicable
										? m.diag_hide_inapplicable()
										: m.diag_show_inapplicable({
												count: inapplicable.length,
											})}
								</button>
								{showInapplicable && (
									<motion.ul
										{...staggerProps(ROW_GAP)}
										className="flex flex-col gap-2"
									>
										{inapplicable.map((check) => (
											<CheckRow key={check.id} check={check} />
										))}
									</motion.ul>
								)}
							</div>
						)}
					</>
				)}
			</CardContent>
		</Card>
	);
};

/**
 * One check. Healthy and inapplicable rows are a single line — there is nothing to act on, and
 * making them expandable would imply otherwise. A row worth acting on opens to the impact and the
 * remedy.
 */
const CheckRow: FC<{ check: HostCheck }> = ({ check }) => {
	const [open, setOpen] = useState(false);
	const expandable = needsAttention(check);
	const title = checkTitle(check);

	const header = (
		<>
			<Badge variant={statusVariant(check)}>{statusLabel(check)}</Badge>
			<span className="font-medium">{title}</span>
			<span className="min-w-0 text-muted-foreground">{check.summary}</span>
		</>
	);

	return (
		<motion.li
			variants={ROW}
			className="rounded-md border bg-card/40 px-3 py-2"
		>
			{expandable ? (
				<button
					type="button"
					className="flex w-full flex-wrap items-baseline gap-x-2 gap-y-1 text-left text-sm"
					aria-expanded={open}
					onClick={() => setOpen((v) => !v)}
				>
					{open ? (
						<ChevronDown className="size-3 self-center" />
					) : (
						<ChevronRight className="size-3 self-center" />
					)}
					{header}
				</button>
			) : (
				<div className="flex flex-wrap items-baseline gap-x-2 gap-y-1 text-sm">
					{header}
				</div>
			)}

			{expandable && open && (
				<div className="mt-3 flex flex-col gap-3 text-sm">
					{check.impact && (
						<div>
							<p className="text-xs text-muted-foreground">
								{m.diag_impact_label()}
							</p>
							<p className="mt-0.5 max-w-prose">{check.impact}</p>
						</div>
					)}
					{check.remedy && (
						<div>
							<p className="text-xs text-muted-foreground">
								{m.diag_remedy_label()}
							</p>
							<p className="mt-0.5 max-w-prose">{check.remedy.text}</p>
							{check.remedy.command && (
								<CopyableCommand command={check.remedy.command} />
							)}
							{check.remedy.relogin_required && (
								<Badge variant="outline" className="mt-2">
									{m.diag_relogin_required()}
								</Badge>
							)}
						</div>
					)}
				</div>
			)}
		</motion.li>
	);
};

const CopyableCommand: FC<{ command: string }> = ({ command }) => (
	<div className="mt-2 flex items-start gap-2">
		<code className="min-w-0 flex-1 overflow-x-auto rounded-md bg-muted px-3 py-1.5 font-mono text-xs text-muted-foreground">
			{command}
		</code>
		<Button
			variant="ghost"
			size="icon"
			title={m.diag_copy()}
			aria-label={m.diag_copy()}
			onClick={() => {
				navigator.clipboard
					.writeText(command)
					.then(() => toast.success(m.diag_copied()))
					.catch(() => toast.error(m.diag_copy_failed()));
			}}
		>
			<Copy className="size-3.5" />
		</Button>
	</div>
);
