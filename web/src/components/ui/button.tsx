import { AnimatedButton, buttonVariants } from "@unom/ui/button";
import type { ComponentProps } from "react";
import { cn } from "@/lib/utils";

// The console's Button IS @unom/ui's animated button — pill shape, specular
// material gloss + UI click/hover sounds (enabled via UnomProviders), driven by
// the shared brand tokens. Same variant/size vocabulary the routes already use
// (default/destructive/outline/secondary/ghost/link + default/sm/lg/icon).
export type ButtonProps = ComponentProps<typeof AnimatedButton>;

/**
 * One correction, in the wrapper layer like the other `components/ui/*` ones: make `disabled`
 * VISIBLE.
 *
 * `AnimatedButton` is a motion element, and its mount animation settles as an inline `opacity: 1`.
 * An inline style outranks any class, so the `disabled:opacity-50` the library also ships never
 * applies: a disabled button stays at `opacity: 1`, looks live and simply ignores the click —
 * `pointer-events: none` still lands, because nothing sets that inline.
 *
 * `!important` is the one thing that beats an inline declaration, and it is preferable here to
 * fighting motion for ownership of the animation: the library keeps animating opacity, this only
 * pins the disabled end state.
 */
export const Button = ({ className, ...props }: ButtonProps) => (
	<AnimatedButton
		className={cn("disabled:opacity-50!", className)}
		{...props}
	/>
);
Button.displayName = "Button";

export { buttonVariants };
