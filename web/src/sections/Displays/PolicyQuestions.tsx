// The display questions, asked one way: Customise asks them of the host, a device's sheet asks
// them of one device with **Follow host** first. Only the axes the host acts on are asked
// (`enforced` / `client_enforced`), so a question here always does something.
import { type FC, type ReactNode, useState } from "react";
import type {
	GameSession,
	Identity,
	KeepAlive,
	ModeConflict,
	Topology,
} from "@/api/gen/model";
import { Input } from "@/components/ui/input";
import { InputNumber } from "@/components/ui/input-number";
import { Segmented } from "@/components/ui/segmented";
import { m } from "@/paraglide/messages";

export interface PolicyValues {
	keep_alive?: KeepAlive;
	topology?: Topology;
	mode_conflict?: ModeConflict;
	identity?: Identity;
	game_session?: GameSession;
	max_mode?: string;
	scale?: number;
}

/** A field set to `null` stops pinning it: the device follows the host again. */
export type PolicyPatch = {
	[K in keyof PolicyValues]?: PolicyValues[K] | null;
};

const FOLLOW = "follow";

export const keepLabel = (k: KeepAlive): string => {
	switch (k.mode) {
		case "off":
			return m.display_q_keep_off();
		case "forever":
			return m.display_q_keep_forever();
		default:
			return m.display_state_kept_for({ seconds: k.seconds });
	}
};

export const topologyLabel = (v: string): string =>
	({
		extend: m.display_q_monitors_extend(),
		primary: m.display_q_monitors_primary(),
		exclusive: m.display_q_monitors_exclusive(),
		auto: m.display_q_monitors_auto(),
	})[v] ?? v;

export const conflictLabel = (v: string): string =>
	({
		separate: m.display_q_second_separate(),
		steal: m.display_q_second_steal(),
		join: m.display_q_second_join(),
		reject: m.display_q_second_reject(),
	})[v] ?? v;

export const identityLabel = (v: string): string =>
	({
		"per-client": m.display_q_remember_client(),
		"per-client-mode": m.display_q_remember_mode(),
		shared: m.display_q_remember_shared(),
	})[v] ?? v;

const gameSessionLabel = (v: string): string =>
	v === "dedicated"
		? m.display_game_session_dedicated()
		: m.display_game_session_auto();

const Question: FC<{ label: string; help?: string; children: ReactNode }> = ({
	label,
	help,
	children,
}) => (
	<fieldset className="space-y-2">
		<legend className="text-sm font-medium">{label}</legend>
		<div className="flex flex-wrap items-center gap-2">{children}</div>
		{help && <p className="text-xs text-muted-foreground">{help}</p>}
	</fieldset>
);

export const PolicyQuestions: FC<{
	/** What is picked. On a device's sheet, absent means it follows the host. */
	value: PolicyValues;
	/** The axes this host acts on, in `enforced` vocabulary. */
	axes: readonly string[];
	onSet: (patch: PolicyPatch) => void;
	/** A device's sheet: the host's answers, each offered first as **Follow host**. */
	inherited?: PolicyValues;
	busy?: boolean;
}> = ({ value, axes, onSet, inherited, busy }) => {
	const keep = value.keep_alive;
	// Kept across Off / Keep so switching back restores the operator's number.
	const [seconds, setSeconds] = useState(
		keep?.mode === "duration" ? keep.seconds : 10,
	);
	/** The device sheet's first answer: what following the host means right now. */
	const follow = (inheritedText?: string) =>
		inherited
			? [
					[
						FOLLOW,
						inheritedText
							? `${m.display_follow_host()} · ${inheritedText}`
							: m.display_follow_host(),
					] as const,
				]
			: [];
	/** One question's segmented answers, with Follow host when this is a device's sheet. */
	const ask = <
		K extends "topology" | "mode_conflict" | "identity" | "game_session",
	>(
		key: K,
		choices: NonNullable<PolicyValues[K]>[],
		label: (v: string) => string,
	) => (
		<Segmented<string>
			busy={busy}
			value={
				(value[key] as string | undefined) ?? (inherited ? FOLLOW : undefined)
			}
			options={[
				...follow(
					inherited?.[key] != null
						? label(inherited[key] as string)
						: undefined,
				),
				...choices.map((c) => [c, label(c)] as const),
			]}
			onPick={(v) => onSet({ [key]: v === FOLLOW ? null : v } as PolicyPatch)}
		/>
	);

	return (
		<div className="space-y-5">
			{axes.includes("keep_alive") && (
				<Question label={m.display_q_keep()}>
					<Segmented<string>
						busy={busy}
						value={keep?.mode ?? (inherited ? FOLLOW : undefined)}
						options={[
							...follow(
								inherited?.keep_alive
									? keepLabel(inherited.keep_alive)
									: undefined,
							),
							["off", m.display_q_keep_off()],
							["duration", m.display_q_keep_for()],
							["forever", m.display_q_keep_forever()],
						]}
						onPick={(mode) =>
							onSet({
								keep_alive:
									mode === FOLLOW
										? null
										: mode === "duration"
											? { mode, seconds }
											: ({ mode } as KeepAlive),
							})
						}
					/>
					{keep?.mode === "duration" && (
						<span className="flex items-center gap-2">
							<InputNumber
								aria-label={m.display_keep_alive_seconds()}
								min={0}
								className="w-24"
								value={keep.seconds}
								disabled={busy}
								onChange={(n) => {
									setSeconds(n);
									onSet({ keep_alive: { mode: "duration", seconds: n } });
								}}
							/>
							<span className="text-sm text-muted-foreground">
								{m.display_keep_alive_seconds()}
							</span>
						</span>
					)}
				</Question>
			)}
			{axes.includes("topology") && (
				<Question label={m.display_q_monitors()}>
					{ask(
						"topology",
						["extend", "primary", "exclusive", "auto"],
						topologyLabel,
					)}
				</Question>
			)}
			{axes.includes("mode_conflict") && (
				<Question label={m.display_q_second()}>
					{ask(
						"mode_conflict",
						["separate", "steal", "join", "reject"],
						conflictLabel,
					)}
				</Question>
			)}
			{axes.includes("identity") && (
				<Question
					label={m.display_q_remember()}
					help={inherited ? undefined : m.display_q_remember_help()}
				>
					{ask(
						"identity",
						["per-client", "per-client-mode", "shared"],
						identityLabel,
					)}
				</Question>
			)}
			{axes.includes("game_session") && (
				<Question
					label={m.display_game_session()}
					help={m.display_game_session_help()}
				>
					{ask("game_session", ["auto", "dedicated"], gameSessionLabel)}
				</Question>
			)}
			{axes.includes("max_mode") && (
				<Question label={m.display_q_max_mode()}>
					<Segmented<string>
						busy={busy}
						value={value.max_mode != null ? undefined : FOLLOW}
						options={follow(m.display_q_max_mode_none())}
						onPick={() => onSet({ max_mode: null })}
					/>
					<Input
						aria-label={m.display_q_max_mode()}
						placeholder="2560x1440@60"
						className="w-40 font-mono"
						defaultValue={value.max_mode ?? ""}
						disabled={busy}
						// On blur: half a mode string is not a cap, and the host refuses to store it.
						onBlur={(e) => {
							const v = e.target.value.trim();
							if (v === (value.max_mode ?? "")) return;
							onSet({ max_mode: v === "" ? null : v });
						}}
					/>
				</Question>
			)}
			{axes.includes("scale") && (
				<Question label={m.display_q_scale()}>
					<Segmented<string | number>
						busy={busy}
						value={value.scale ?? FOLLOW}
						options={[
							...follow(m.display_q_scale_desktop()),
							...[1, 1.25, 1.5, 2].map((v) => [v, `${v}×`] as const),
						]}
						onPick={(v) =>
							onSet({ scale: v === FOLLOW ? null : (v as number) })
						}
					/>
				</Question>
			)}
		</div>
	);
};
