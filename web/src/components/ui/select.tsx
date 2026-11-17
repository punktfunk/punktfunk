// The console's Select IS @unom/ui's radix select, with two corrections of the kind the Tabs
// wrapper makes — @unom/ui's palette names don't all mean the same thing in this app's token set:
//
//   • `text-secondary`, which the trigger uses for BOTH the placeholder and the chevron, is a
//     *text* colour upstream. Here `--secondary` is a SURFACE (styles.css), so the chevron
//     would render at near-zero contrast against the card it sits on — a select that looks like
//     a plain box with no affordance at all. `text-muted-foreground` is this app's "quiet text".
//   • `border-main` is the foreground colour — a near-white 1px border in dark, which would make a
//     select shout next to the `border-input` used by every Input beside it.
//
// The trigger also defaults to `w-full` (upstream is `w-fit`) and to the Input's `rounded-md`: in
// this console a select is a form field in a stacked column, never an inline chip.
//
// Same shape as the other `components/ui/*` wrappers: adapt the shared primitive to this app's
// tokens once, rather than restyling it at every call site.
import {
	Select,
	SelectContent,
	SelectGroup,
	SelectItem as SelectItemBase,
	SelectLabel,
	SelectScrollDownButton,
	SelectScrollUpButton,
	SelectSeparator,
	SelectTrigger as SelectTriggerBase,
	SelectValue,
} from "@unom/ui/form/select";
import type { ComponentProps } from "react";
import { cn } from "@/lib/utils";

const SelectTrigger = ({
	className,
	...props
}: ComponentProps<typeof SelectTriggerBase>) => (
	<SelectTriggerBase
		className={cn(
			"w-full rounded-md border-input data-placeholder:text-muted-foreground",
			"[&_svg:not([class*='text-'])]:text-muted-foreground",
			className,
		)}
		{...props}
	/>
);
SelectTrigger.displayName = "SelectTrigger";

// The highlighted row. Upstream paints it `bg-main/25` — a neutral grey wash, since `--main` is the
// foreground colour. Everywhere else in this console the "this is the one" wash is brand violet
// (`bg-primary/15` on the nav's hover and active states, `border-primary` on a chosen preset
// card), so a grey row is the odd one out the moment a select sits next to any of them.
const SelectItem = ({
	className,
	...props
}: ComponentProps<typeof SelectItemBase>) => (
	<SelectItemBase
		className={cn("focus:bg-primary/15 focus:text-foreground", className)}
		{...props}
	/>
);
SelectItem.displayName = "SelectItem";

export {
	Select,
	SelectContent,
	SelectGroup,
	SelectItem,
	SelectLabel,
	SelectScrollDownButton,
	SelectScrollUpButton,
	SelectSeparator,
	SelectTrigger,
	SelectValue,
};
