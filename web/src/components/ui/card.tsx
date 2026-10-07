import { AnimatedCard } from "@unom/ui/card";
import type { ComponentProps } from "react";
import * as React from "react";
import { cn } from "@/lib/utils";

// The console's Card IS @unom/ui's animated card — a `bg-neutral` (#1c1530)
// surface with a soft brand-violet ring, on-mount motion + material gloss
// (enabled via UnomProviders). We keep the composed shadcn-style sub-component
// API (CardHeader/Title/Description/Content/Footer own their own padding), so
// the card defaults to `padding={false}` to avoid doubling it, and soften the
// 2px ring to a subtle 1px brand tint. `pf-card` lets the phone layout find a
// page-level card (styles.css).
type CardProps = ComponentProps<typeof AnimatedCard>;

const Card = ({
	className,
	padding = false,
	children,
	...props
}: CardProps) => (
	<AnimatedCard
		padding={padding}
		className={cn("pf-card ring-1 ring-accent/40", className)}
		{...props}
	>
		{children}
	</AnimatedCard>
);
Card.displayName = "Card";

/**
 * The card inset, as ONE utility.
 *
 * It used to be `p-4 sm:p-6`, and that responsive pair is what made every padding override in this
 * codebase unreliable: tailwind-merge resolves conflicts only *within* a variant, so a call-site
 * `pt-6` beat the base `pt-0` and lost to `sm:pt-0` — correct on mobile, zero on desktop. Seven call
 * sites had grown their own compensation for that in five different dialects.
 *
 * A single-variant token cannot half-lose. `--spacing-padding-card` is also what @unom/ui's own
 * `Card` uses, so nested cards finally agree on their inset.
 */
const INSET = "p-padding-card";

/**
 * Body/footer padding, minus the top when something already sits above.
 *
 * The old code hard-coded `pt-0` because "a CardHeader supplies the top inset" — an assumption about
 * a SIBLING that nothing enforced. Delete the header (exactly what tabbing a page does, since the
 * tab label replaces the card title) and the top inset silently vanished at ≥640px. Asking the DOM
 * instead of the author makes it self-correcting: first child keeps its inset, later children drop
 * it.
 */
const INSET_AFTER_SIBLING = `${INSET} [&:not(:first-child)]:pt-0`;

const CardHeader = React.forwardRef<
	HTMLDivElement,
	React.HTMLAttributes<HTMLDivElement>
>(({ className, ...props }, ref) => (
	<div
		ref={ref}
		className={cn("flex flex-col space-y-1.5", INSET, className)}
		{...props}
	/>
));
CardHeader.displayName = "CardHeader";

const CardTitle = React.forwardRef<
	HTMLDivElement,
	React.HTMLAttributes<HTMLDivElement>
>(({ className, ...props }, ref) => (
	<div
		ref={ref}
		className={cn("font-semibold leading-none tracking-tight", className)}
		{...props}
	/>
));
CardTitle.displayName = "CardTitle";

const CardDescription = React.forwardRef<
	HTMLDivElement,
	React.HTMLAttributes<HTMLDivElement>
>(({ className, ...props }, ref) => (
	<div
		ref={ref}
		className={cn("text-sm text-muted-foreground", className)}
		{...props}
	/>
));
CardDescription.displayName = "CardDescription";

/**
 * Card body. Pass `flush` for content that should meet the card's edges — a full-bleed table, most
 * commonly — instead of trying to cancel the padding from the outside.
 *
 * Do NOT reach for `className="p-0"`: `flush` exists precisely so that intent is expressed as a prop
 * the component honours, rather than as a utility that has to out-argue the one already there.
 *
 * Conversely, you no longer need to ADD top padding when there is no header — that is automatic now.
 * If you find yourself writing `pt-*` on a CardContent, the layout is telling you something else is
 * wrong.
 */
const CardContent = React.forwardRef<
	HTMLDivElement,
	React.HTMLAttributes<HTMLDivElement> & { flush?: boolean }
>(({ className, flush = false, ...props }, ref) => (
	<div
		ref={ref}
		className={cn(!flush && INSET_AFTER_SIBLING, className)}
		{...props}
	/>
));
CardContent.displayName = "CardContent";

const CardFooter = React.forwardRef<
	HTMLDivElement,
	React.HTMLAttributes<HTMLDivElement>
>(({ className, ...props }, ref) => (
	<div
		ref={ref}
		className={cn("flex items-center", INSET_AFTER_SIBLING, className)}
		{...props}
	/>
));
CardFooter.displayName = "CardFooter";

export {
	Card,
	CardContent,
	CardDescription,
	CardFooter,
	CardHeader,
	CardTitle,
};
