// Settings → Appearance (design/web-console-overhaul.md §7.2).
//
// "Follow host" names the source it is following and shows the swatch, so the default explains
// itself instead of leaving the operator to guess what it resolved to. On a box where nothing
// answered it says so outright rather than pretending to follow something.
//
// A change reloads the page. The theme is expanded by CSS from attributes on `<html>`, which the
// server render sets from the cookie — so re-rendering from the server is both the simplest way
// to apply it and the one that proves the cookie round-trips.

import { Palette } from "lucide-react";
import type { FC } from "react";
import { useState } from "react";
import { useUiConfig } from "@/api/uiConfig";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import {
	ACCENT_SWATCHES,
	type Appearance,
	isSafeColor,
	readAppearance,
	writeAppearance,
} from "@/lib/appearance";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";

export const AppearanceCard: FC = () => {
	const { data: uiConfig } = useUiConfig();
	const theme = uiConfig?.theme ?? null;
	const [appearance, setAppearance] = useState<Appearance>(readAppearance);
	const [custom, setCustom] = useState("");

	const apply = (next: Appearance) => {
		setAppearance(next);
		writeAppearance(next);
		// The attributes are set during the server render; reloading is what applies them.
		window.location.reload();
	};

	// What "Follow host" resolves to right now — the whole point of naming it.
	const followsMode = theme?.mode ?? null;
	const followsAccent = theme?.accent ?? null;
	const sourceLabel = theme?.source
		? m.appearance_following({ source: theme.source })
		: m.appearance_none_detected();

	return (
		<Card className="max-w-lg">
			<CardHeader>
				<CardTitle className="flex items-center gap-2">
					<Palette className="size-4" />
					{m.settings_appearance()}
				</CardTitle>
			</CardHeader>
			<CardContent className="space-y-5">
				<Row label={m.appearance_theme()}>
					<Choice
						selected={appearance.mode === "system"}
						onPick={() => apply({ ...appearance, mode: "system" })}
					>
						{m.appearance_follow_host()}
						<span className="opacity-70">
							{" · "}
							{followsMode ? modeLabel(followsMode) : sourceLabel}
						</span>
					</Choice>
					<Choice
						selected={appearance.mode === "light"}
						onPick={() => apply({ ...appearance, mode: "light" })}
					>
						{m.appearance_light()}
					</Choice>
					<Choice
						selected={appearance.mode === "dark"}
						onPick={() => apply({ ...appearance, mode: "dark" })}
					>
						{m.appearance_dark()}
					</Choice>
				</Row>

				<Row label={m.appearance_accent()}>
					<Choice
						selected={appearance.accent === "system"}
						onPick={() => apply({ ...appearance, accent: "system" })}
					>
						{m.appearance_follow_host()}
						{followsAccent ? (
							<Swatch color={followsAccent} />
						) : (
							<span className="opacity-70">
								{" · "}
								{sourceLabel}
							</span>
						)}
					</Choice>
					{ACCENT_SWATCHES.map((c) => (
						<button
							key={c}
							type="button"
							aria-label={c}
							aria-pressed={appearance.accent === c}
							onClick={() => apply({ ...appearance, accent: c })}
							className={cn(
								"size-7 rounded-full border transition-transform hover:scale-110",
								appearance.accent === c && "ring-2 ring-ring ring-offset-2",
							)}
							style={{ background: c }}
						/>
					))}
				</Row>

				<div className="flex flex-wrap items-center gap-2">
					<Input
						aria-label={m.appearance_custom()}
						placeholder="#6c5bf3"
						className="w-32 font-mono"
						value={custom}
						onChange={(e) => setCustom(e.target.value)}
					/>
					{/* Validated here as well as on read: this value ends up in a style attribute,
					    which is the one security line on this panel. */}
					<Button
						size="input"
						variant="outline"
						disabled={!isSafeColor(custom)}
						onClick={() => apply({ ...appearance, accent: custom })}
					>
						{m.appearance_use_custom()}
					</Button>
				</div>
			</CardContent>
		</Card>
	);
};

const Row: FC<{ label: string; children: React.ReactNode }> = ({
	label,
	children,
}) => (
	<fieldset className="space-y-2">
		<legend className="text-sm font-medium">{label}</legend>
		<div className="flex flex-wrap items-center gap-2">{children}</div>
	</fieldset>
);

const Choice: FC<{
	selected: boolean;
	onPick: () => void;
	children: React.ReactNode;
}> = ({ selected, onPick, children }) => (
	<Button
		size="sm"
		variant={selected ? "default" : "outline"}
		aria-pressed={selected}
		onClick={onPick}
	>
		{children}
	</Button>
);

const Swatch: FC<{ color: string }> = ({ color }) => (
	<span
		aria-hidden
		className="ml-1.5 size-3.5 rounded-full border"
		style={{ background: color }}
	/>
);

const modeLabel = (mode: string) =>
	mode === "light" ? m.appearance_light() : m.appearance_dark();
