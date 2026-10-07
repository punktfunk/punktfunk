// **Advanced**, at the foot of the Displays page: the monitor levers a host acts on, the screen
// cap, and the way back from a manual arrangement.
//
// `<details>` rather than a state-driven accordion: the browser ships the open/close, the keyboard
// handling and the aria wiring, and a closed section costs one row.
import type { FC, ReactNode } from "react";
import type { DisplayPolicy, EffectivePolicy } from "@/api/gen/model";
import { usePlatform } from "@/api/platform";
import { DocsLink } from "@/components/docs-link";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { InputNumber } from "@/components/ui/input-number";
import { Segmented } from "@/components/ui/segmented";
import { m } from "@/paraglide/messages";

const Disclosure: FC<{ label: string; children: ReactNode }> = ({
	label,
	children,
}) => (
	<Card>
		<CardContent flush className="p-0">
			<details className="group">
				<summary className="cursor-pointer list-none px-4 py-3 text-sm font-medium marker:content-none">
					<span className="inline-block transition-transform group-open:rotate-90">
						▸
					</span>{" "}
					{label}
				</summary>
				<div className="space-y-5 border-t px-4 py-4">{children}</div>
			</details>
		</CardContent>
	</Card>
);

const Field: FC<{ label: string; help?: ReactNode; children: ReactNode }> = ({
	label,
	help,
	children,
}) => (
	<fieldset className="space-y-2">
		<legend className="text-sm font-medium">{label}</legend>
		{children}
		{help && (
			<p className="max-w-prose text-xs text-muted-foreground">{help}</p>
		)}
	</fieldset>
);

/** The Windows exclusive-isolate levers render only where the host acts on one (D1). */
export const AdvancedDisclosure: FC<{
	policy?: DisplayPolicy;
	effective?: EffectivePolicy;
	busy?: boolean;
	onSet: (patch: Partial<DisplayPolicy>) => void;
	/** Writes a preset field: switches the policy to Custom first. */
	onSetField: (patch: Partial<DisplayPolicy>) => void;
}> = ({ policy, effective, busy, onSet, onSetField }) => {
	const { acts } = usePlatform();
	const levers = (
		[
			[
				"ddc_power_off",
				m.display_ddc(),
				m.display_ddc_help(),
				"virtual-displays#power-monitors-off-ddcci",
			],
			[
				"pnp_disable_monitors",
				m.display_pnp(),
				m.display_pnp_help(),
				"virtual-displays#disable-monitor-devices-pnp",
			],
			[
				"edid_lock",
				m.display_edid(),
				m.display_edid_help(),
				"virtual-displays#hold-monitor-identity-edid",
			],
		] as const
	).filter(([field]) => acts("display", field));
	const manual = effective?.layout.mode === "manual";
	return (
		<Disclosure label={m.display_advanced()}>
			{levers.map(([field, label, help, docs]) => (
				<Field
					key={field}
					label={label}
					help={
						<>
							{help} <DocsLink path={docs} />
						</>
					}
				>
					<Segmented
						busy={busy}
						value={policy?.[field] ?? false}
						options={[
							[false, m.common_off()],
							[true, m.common_on()],
						]}
						onPick={(on) => onSet({ [field]: on })}
					/>
				</Field>
			))}
			{effective && (
				// 1..=16 is the host's own clamp on write.
				<Field label={m.display_q_max()}>
					<InputNumber
						min={1}
						max={16}
						className="w-24"
						aria-label={m.display_q_max()}
						value={effective.max_displays}
						disabled={busy}
						onChange={(max_displays) => onSetField({ max_displays })}
					/>
				</Field>
			)}
			{manual && (
				<Field label={m.display_arranged_by_you()}>
					<Button
						size="sm"
						variant="outline"
						className="self-start"
						disabled={busy}
						onClick={() =>
							onSet({
								layout: { ...policy?.layout, mode: "auto-row" },
							})
						}
					>
						{m.display_arrange_auto()}
					</Button>
				</Field>
			)}
		</Disclosure>
	);
};
