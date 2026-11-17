import type { Meta, StoryObj } from "@storybook/react-vite";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
	Card,
	CardContent,
	CardDescription,
	CardFooter,
	CardHeader,
	CardTitle,
} from "@/components/ui/card";

const meta = {
	title: "UI/Card",
	component: Card,
	// Card requires `children`; every story supplies its own via `render`, so this
	// is just a placeholder to satisfy the arg type.
	args: { children: null },
} satisfies Meta<typeof Card>;

export default meta;
type Story = StoryObj<typeof meta>;

/**
 * The inset contract.
 *
 * `CardContent` drops its top padding only when something already sits above it. The pair below is
 * the regression guard: both cards must show the same inset on every side, and the headerless one
 * must not have its first line touching the top edge.
 *
 * ⚠ Check this at BOTH viewport widths: an inset bug can show at one width and not the other.
 */
export const InsetWithAndWithoutHeader: Story = {
	render: () => (
		<div className="grid gap-4 sm:grid-cols-2">
			<Card>
				<CardHeader>
					<CardTitle>With a header</CardTitle>
				</CardHeader>
				<CardContent className="text-sm text-muted-foreground">
					The body drops its top inset because the header above already supplied
					one.
				</CardContent>
			</Card>
			<Card>
				<CardContent className="text-sm text-muted-foreground">
					No header, so the body keeps its own top inset — automatically, with
					nothing for the call site to remember.
				</CardContent>
			</Card>
		</div>
	),
};

export const HostCard: Story = {
	render: () => (
		<Card className="max-w-sm">
			<CardHeader>
				<div className="flex items-center justify-between">
					<CardTitle>ENRICOS-DESKTOP</CardTitle>
					<Badge variant="success">online</Badge>
				</div>
				<CardDescription>RTX 5070 Ti · NVENC · 5120×1440 @ 240</CardDescription>
			</CardHeader>
			<CardContent className="text-sm text-muted-foreground">
				Paired 2 days ago. Last session 11 ms p50 capture→present.
			</CardContent>
			<CardFooter className="gap-2">
				<Button size="sm">Connect</Button>
				<Button size="sm" variant="outline">
					Details
				</Button>
			</CardFooter>
		</Card>
	),
};
