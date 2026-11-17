import type { Meta, StoryObj } from "@storybook/react-vite";
import { useEffect, useState } from "react";
import { Stagger } from "@/components/stagger";
import { Card, CardContent } from "@/components/ui/card";

/**
 * The on-mount cadence (`components/stagger.tsx`).
 *
 * Here because a stagger that does not run is invisible in a screenshot — a flattened group and
 * a staggered one are identical once they settle. Judge these by watching, or by sampling opacity
 * mid-entrance.
 *
 * `AfterLoading` records something worth keeping: a group that mounts half a second late, the way
 * a query resolves, still staggers. A child mounting into an ancestor already in `enter` runs its
 * own `from → enter`. So arriving after the page is NOT by itself what breaks a group, and `root`
 * is not the reflex for it — a portal or a tab panel is.
 *
 * Each of these must stagger. Storybook wraps every story in a `<Section>`, so a story cannot tell
 * you whether the real page has an ancestor driving at all; that part only shows up in the app.
 */
const tiles = ["One", "Two", "Three", "Four", "Five", "Six"];

const GRID = "grid grid-cols-2 gap-card sm:grid-cols-3";

const Tiles = () => (
	<>
		{tiles.map((t) => (
			<Card key={t}>
				<CardContent>
					<p className="font-medium">{t}</p>
					<p className="text-sm text-muted-foreground">A tile that arrives.</p>
				</CardContent>
			</Card>
		))}
	</>
);

const meta = {
	title: "Console/Stagger",
	parameters: { layout: "padded" },
} satisfies Meta;
export default meta;

type Story = StoryObj<typeof meta>;

/** The cadence, as every card group should have it. */
export const Wrapped: Story = {
	render: () => (
		<div className="max-w-3xl">
			<Stagger className={GRID}>
				<Tiles />
			</Stagger>
		</div>
	),
};

/** Mounted half a second late, the way a query resolves. Still staggers. */
export const AfterLoading: Story = {
	render: function Late() {
		const [ready, setReady] = useState(false);
		useEffect(() => {
			const t = setTimeout(() => setReady(true), 500);
			return () => clearTimeout(t);
		}, []);
		return (
			<div className="max-w-3xl">
				{ready ? (
					<Stagger className={GRID}>
						<Tiles />
					</Stagger>
				) : (
					<p className="text-sm text-muted-foreground">Loading…</p>
				)}
			</div>
		);
	},
};

/** `root`, the way a portal or a tab panel needs it — no ancestor to inherit from. */
export const SelfDriven: Story = {
	render: () => (
		<div className="max-w-3xl">
			<Stagger root className={GRID}>
				<Tiles />
			</Stagger>
		</div>
	),
};
