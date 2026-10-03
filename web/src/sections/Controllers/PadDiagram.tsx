// One controller drawing, lit from one `PadFrame`.
//
// The art is `PAD_ART`, generated from the assets/pads masters the console draws too. Every
// fill is the foreground mixed into the card, opaque, so a bumper hides the trigger behind it;
// whatever is held turns primary.

import { type FC, useId } from "react";
import type { PadFrame } from "@/api/gen/model/padFrame";
import { PAD_ART, type PadPart } from "./padArt";
import { ART_BITS } from "./pads";

const ACCENT = "var(--primary)";
const ON_ACCENT = "var(--primary-foreground)";
/** `share` of `color` mixed into the card: opaque, and it follows the theme. */
const mix = (color: string, share: number) =>
	`color-mix(in srgb, ${color} ${Math.round(share * 100)}%, var(--card))`;
const fg = (share: number) => mix("currentColor", share);
const ink = (on: boolean) => (on ? ON_ACCENT : fg(0.75));
/** Hairlines stay one weight at any size. */
const HAIR = { vectorEffect: "non-scaling-stroke" } as const;
const GLOW = `drop-shadow(0 0 1.5px ${ACCENT})`;
/** A cap's radius and its travel, as fractions of the well's. */
const CAP = 0.72;
const TRAVEL = 0.42;

interface Reading {
	held: (id: string) => boolean;
	/** A trigger's pull, 0…1. */
	pull: (id: string) => number;
	/** A stick's deflection, −1…1 each, +y down. */
	tilt: (id: string) => [number, number];
}

const axis = (v: number) => Math.max(-1, Math.min(1, v / 32767));

export const PadDiagram: FC<{ frame: PadFrame }> = ({ frame }) => {
	const uid = useId();
	// Auto, or a kind newer than this console: the host's default build.
	const art = PAD_ART[frame.device] ?? PAD_ART.xbox360;
	if (!art) return null;
	const reading: Reading = {
		held: (id) => (frame.buttons & (ART_BITS[id] ?? 0)) !== 0,
		pull: (id) =>
			(id === "LT"
				? frame.left_trigger
				: id === "RT"
					? frame.right_trigger
					: 0) / 255,
		// The wire's +y is up.
		tilt: (id) =>
			id === "LS"
				? [axis(frame.ls_x), -axis(frame.ls_y)]
				: id === "RS"
					? [axis(frame.rs_x), -axis(frame.rs_y)]
					: [0, 0],
	};
	return (
		<svg
			viewBox={`0 0 ${art.w} ${art.h}`}
			role="img"
			aria-label={`Pad ${frame.pad}: ${art.name}`}
			className="mx-auto w-full max-w-xl text-foreground"
			strokeLinecap="round"
			strokeLinejoin="round"
		>
			<defs>
				{/* The shell catches the light from above. */}
				<linearGradient id={`${uid}-shell`} x1="0" y1="0" x2="0" y2="1">
					<stop offset={0} style={{ stopColor: fg(0.11) }} />
					<stop offset={1} style={{ stopColor: fg(0.04) }} />
				</linearGradient>
			</defs>
			{art.parts.map((p, i) => (
				<Part key={i} part={p} reading={reading} uid={uid} n={i} />
			))}
		</svg>
	);
};

const Part: FC<{ part: PadPart; reading: Reading; uid: string; n: number }> = ({
	part: p,
	reading: r,
	uid,
	n,
}) => {
	switch (p.k) {
		case "body":
			return (
				<path
					d={p.d}
					fill={`url(#${uid}-shell)`}
					strokeWidth={1.5}
					style={{ ...HAIR, stroke: fg(0.32) }}
				/>
			);
		case "panel":
			return <path d={p.d} style={{ fill: mix("#000", 0.25) }} />;
		case "line":
			return (
				<path
					d={p.d}
					fill="none"
					strokeWidth={1}
					style={{ ...HAIR, stroke: fg(0.22) }}
				/>
			);
		case "button": {
			const on = r.held(p.id);
			return (
				<path
					d={p.d}
					strokeWidth={1}
					style={{
						...HAIR,
						fill: on ? ACCENT : fg(0.14),
						stroke: on ? ACCENT : fg(0.3),
						filter: on ? GLOW : undefined,
					}}
				/>
			);
		}
		case "trigger": {
			// Fills from the tip as it is pulled.
			const pull = Math.max(r.held(p.id) ? 1 : 0, r.pull(p.id));
			return (
				<>
					<linearGradient id={`${uid}-${n}`} x1="0" y1="0" x2="0" y2="1">
						<stop offset={pull} style={{ stopColor: ACCENT }} />
						<stop offset={pull} style={{ stopColor: fg(0.14) }} />
					</linearGradient>
					<path
						d={p.d}
						fill={`url(#${uid}-${n})`}
						strokeWidth={1}
						style={{ ...HAIR, stroke: pull > 0 ? ACCENT : fg(0.3) }}
					/>
				</>
			);
		}
		case "stick": {
			const on = r.held(p.id);
			const [x, y] = r.tilt(p.id);
			const travel = p.r * (p.pad ? 0.8 : TRAVEL);
			const cx = p.cx + x * travel;
			const cy = p.cy + y * travel;
			return (
				<g>
					<circle
						cx={p.cx}
						cy={p.cy}
						r={p.r}
						strokeWidth={1}
						style={{
							...HAIR,
							fill: on && p.pad ? mix(ACCENT, 0.4) : mix("#000", 0.32),
							stroke: fg(0.18),
						}}
					/>
					{p.pad ? (
						(x !== 0 || y !== 0) && (
							<circle cx={cx} cy={cy} r={p.r * 0.16} style={{ fill: ACCENT }} />
						)
					) : (
						<>
							<circle
								cx={cx}
								cy={cy}
								r={p.r * CAP}
								strokeWidth={1}
								style={{
									...HAIR,
									fill: on ? ACCENT : fg(0.24),
									stroke: on ? ACCENT : fg(0.45),
									filter: on ? GLOW : undefined,
								}}
							/>
							{/* The cap's dish. */}
							<circle
								cx={cx}
								cy={cy}
								r={p.r * CAP * 0.58}
								fill="none"
								strokeWidth={1}
								style={{ ...HAIR, stroke: on ? ON_ACCENT : fg(0.12) }}
							/>
						</>
					)}
				</g>
			);
		}
		case "glyph":
			return (
				<path
					d={p.d}
					fill="none"
					strokeWidth={p.w}
					style={{ stroke: ink(r.held(p.on)) }}
				/>
			);
		case "mark":
			return <path d={p.d} style={{ fill: ink(r.held(p.on)) }} />;
		case "label":
			return (
				<text
					x={p.x}
					y={p.y}
					fontSize={p.size}
					fontWeight={600}
					textAnchor="middle"
					dominantBaseline="central"
					style={{ fill: ink(r.held(p.on)) }}
				>
					{p.text}
				</text>
			);
	}
};
