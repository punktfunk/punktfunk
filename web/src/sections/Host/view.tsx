import { Link } from "@tanstack/react-router";
import Section from "@unom/ui/section";
import {
	ArrowRight,
	Film,
	IdCard,
	Layers,
	Network,
	SlidersHorizontal,
} from "lucide-react";
import { motion } from "motion/react";
import type { FC, ReactNode } from "react";
import type { AvailableCompositor } from "@/api/gen/model/availableCompositor";
import type { HostInfo } from "@/api/gen/model/hostInfo";
import { OsIcon } from "@/components/os-icon";
import { QueryState } from "@/components/query-state";
import { ROW, ROW_GAP, Stagger, staggerProps } from "@/components/stagger";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import type { Loadable } from "@/lib/query";
import { m } from "@/paraglide/messages";
import { ConnectCard } from "./ConnectCard";

export const HostView: FC<{
	host: Loadable<HostInfo>;
	compositors: Loadable<AvailableCompositor[]>;
	/** The GPU inventory/selection card (a self-contained container — see `GpuCard.tsx`). */
	gpu?: ReactNode;
	/** The update-check card (a self-contained container — see `UpdateCard.tsx`). */
	update?: ReactNode;
	/** The host-power actions card (a self-contained container — see `PowerCard.tsx`). */
	power?: ReactNode;
	/** Warning about other Moonlight-compatible servers on this machine — renders nothing when
	 * there are none (see `ConflictsCard.tsx`). Sits at the top: it explains "nothing can connect". */
	conflicts?: ReactNode;
	/** Which endpoints carry game audio and the microphone (Windows; see `AudioWiring.tsx`). */
	audio?: ReactNode;
}> = ({ host, compositors, gpu, update, power, conflicts, audio }) => {
	const h = host.data;
	return (
		<Section maxWidth={false}>
			<div className="flex flex-col gap-card">
				<h1 className="text-2xl font-semibold">{m.nav_host()}</h1>

				{conflicts}

				<Card>
					<CardContent className="flex flex-wrap items-center gap-3">
						<SlidersHorizontal className="size-4 text-muted-foreground" />
						<div className="min-w-0 flex-1">
							<h2 className="font-medium">{m.host_settings_title()}</h2>
							<p className="text-sm text-muted-foreground">
								{m.host_settings_card_hint()}
							</p>
						</div>
						<Button asChild size="sm" variant="outline">
							<Link to="/host/settings">
								{m.host_settings_open()}
								<ArrowRight className="size-4" />
							</Link>
						</Button>
					</CardContent>
				</Card>

				<QueryState
					isLoading={host.isLoading}
					error={host.error}
					refetch={host.refetch}
				>
					{/* One group, mounted with its cards once /host answers — a late group takes its cadence from its own container. */}
					{h && (
						<Stagger className="flex flex-col gap-card">
							<ConnectCard host={h} />
							<div className="grid gap-card lg:grid-cols-2">
								<Card>
									<CardHeader>
										<CardTitle className="flex items-center gap-2">
											<IdCard className="size-4" />
											{m.host_identity()}
										</CardTitle>
									</CardHeader>
									<CardContent>
										<dl className="grid grid-cols-1 gap-3">
											<Row label={m.host_hostname()} value={h.hostname} />
											{/* The OS mark resolves from the identity chain (h.os), which also
										    serves as the tooltip for the curious; the text is the pretty name. */}
											<Row
												label={m.host_os()}
												value={h.os_name}
												title={h.os}
												icon={<OsIcon os={h.os} className="size-4 shrink-0" />}
											/>
											<Row label={m.host_local_ip()} value={h.local_ip} mono />
											<Row
												label={m.host_version()}
												value={`${h.app_version} (${h.version})`}
											/>
											<Row label={m.host_abi()} value={String(h.abi_version)} />
											<Row label={m.host_uniqueid()} value={h.uniqueid} mono />
										</dl>
									</CardContent>
								</Card>
								<div className="space-y-card">
									<Card>
										<CardHeader>
											<CardTitle className="flex items-center gap-2">
												<Film className="size-4" />
												{m.host_codecs()}
											</CardTitle>
										</CardHeader>
										<CardContent className="flex flex-wrap gap-2">
											{h.codecs.map((c) => (
												<Badge key={c} variant="secondary">
													{c.toUpperCase()}
												</Badge>
											))}
										</CardContent>
									</Card>
									<Card>
										<CardHeader>
											<CardTitle className="flex items-center gap-2">
												<Network className="size-4" />
												{m.host_ports()}
											</CardTitle>
										</CardHeader>
										<CardContent>
											<dl className="grid grid-cols-2 gap-x-6 gap-y-2 text-sm tabular-nums">
												{Object.entries(h.ports).map(([k, v]) => (
													<div key={k} className="flex justify-between">
														<dt className="text-muted-foreground uppercase">
															{k}
														</dt>
														<dd className="font-medium">{v as number}</dd>
													</div>
												))}
											</dl>
										</CardContent>
									</Card>
								</div>
							</div>
						</Stagger>
					)}
				</QueryState>

				{audio}

				{update}

				{power}

				{gpu}

				{/* Empty is a real answer, not a load failure: a Windows host drives the
				    pf-vdisplay driver and has no compositor backends at all — so the card is
				    absent there rather than reporting "none" at a card's worth of height. */}
				{compositors.data?.length !== 0 && (
					<Card>
						<CardHeader>
							<CardTitle className="flex items-center gap-2">
								<Layers className="size-4" />
								{m.host_compositors()}
							</CardTitle>
						</CardHeader>
						<CardContent className="space-y-4">
							<p className="text-sm text-muted-foreground">
								{m.host_compositors_help()}
							</p>
							<QueryState
								isLoading={compositors.isLoading}
								error={compositors.error}
								refetch={compositors.refetch}
							>
								<motion.ul
									{...staggerProps(ROW_GAP)}
									className="divide-y rounded-md border"
								>
									{compositors.data?.map((c) => (
										<motion.li
											variants={ROW}
											key={c.id}
											className="flex items-center justify-between gap-4 px-4 py-3"
										>
											<div className="min-w-0">
												<div className="flex items-center gap-2">
													<span className="font-medium">{c.label}</span>
													{c.default && (
														<Badge variant="secondary">
															{m.compositor_default()}
														</Badge>
													)}
												</div>
												<code className="text-xs text-muted-foreground">
													{c.id}
												</code>
											</div>
											<Badge variant={c.available ? "default" : "outline"}>
												{c.available
													? m.compositor_available()
													: m.compositor_unavailable()}
											</Badge>
										</motion.li>
									))}
								</motion.ul>
							</QueryState>
						</CardContent>
					</Card>
				)}
			</div>
		</Section>
	);
};

const Row: FC<{
	label: string;
	value: string;
	mono?: boolean;
	/** Optional leading glyph inside the value cell (the OS mark). */
	icon?: ReactNode;
	/** Tooltip override — defaults to the value itself (which may be truncated). */
	title?: string;
}> = ({ label, value, mono, icon, title }) => (
	<div className="flex items-baseline justify-between gap-4">
		<dt className="text-sm text-muted-foreground">{label}</dt>
		<dd
			className={`${mono ? "truncate font-mono text-xs" : "font-medium"}${icon ? " flex items-center gap-2" : ""}`}
			title={title ?? value}
		>
			{icon}
			{value}
		</dd>
	</div>
);
