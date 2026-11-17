// The console's Tabs ARE @unom/ui's radix tabs, with one correction: the inactive trigger colour.
//
// @unom/ui styles inactive triggers `text-secondary` and the active one `data-[state=active]:
// text-main`. Those tokens come from @unom/ui's own palette; in the console's theme `text-secondary`
// lands on (near) the tab strip's own background, so every inactive tab renders as an invisible
// gap — the tab bar looks like a single lonely label with dead space beside it. Types do not
// catch it: the markup and the a11y roles are entirely correct.
//
// Same shape as the other `components/ui/*` wrappers: adapt the shared primitive to this app's
// tokens rather than restyling it at every call site.
import {
	Tabs,
	TabsContent,
	TabsList,
	TabsTrigger as TabsTriggerBase,
} from "@unom/ui/tabs";
import type { ComponentProps } from "react";
import { cn } from "@/lib/utils";

const TabsTrigger = ({
	className,
	...props
}: ComponentProps<typeof TabsTriggerBase>) => (
	<TabsTriggerBase
		className={cn(
			"text-muted-foreground hover:text-foreground data-[state=active]:text-foreground",
			className,
		)}
		{...props}
	/>
);
TabsTrigger.displayName = "TabsTrigger";

export { Tabs, TabsContent, TabsList, TabsTrigger };
