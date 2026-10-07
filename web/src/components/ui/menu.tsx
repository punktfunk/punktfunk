// A row's ⋯: a row shows at most two actions, the rest live here (design/web-console-structure-
// 2026-10.md R6). Radix's dropdown menu in the console's tokens, like the Select wrapper.
import { Check, ChevronRight, MoreHorizontal } from "lucide-react";
import { DropdownMenu as M } from "radix-ui";
import type { ComponentProps, FC, ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

const CONTENT =
	"z-50 min-w-48 overflow-hidden rounded-md border bg-popover p-1 text-popover-foreground shadow-md data-[state=closed]:animate-out data-[state=closed]:fade-out-0 data-[state=open]:animate-in data-[state=open]:fade-in-0";
const ITEM =
	"relative flex cursor-default select-none items-center gap-2 rounded-sm px-2 py-1.5 text-sm outline-none focus:bg-primary/15 data-[disabled]:pointer-events-none data-[disabled]:opacity-50 data-[state=open]:bg-primary/15 [&_svg]:size-4 [&_svg]:shrink-0";

/** The ⋯ button and its menu. */
export const RowMenu: FC<{
	label: string;
	disabled?: boolean;
	children: ReactNode;
}> = ({ label, disabled, children }) => (
	<M.Root modal={false}>
		<M.Trigger asChild disabled={disabled}>
			<Button variant="ghost" size="icon" aria-label={label} title={label}>
				<MoreHorizontal className="size-4" />
			</Button>
		</M.Trigger>
		<M.Portal>
			<M.Content align="end" sideOffset={4} className={CONTENT}>
				{children}
			</M.Content>
		</M.Portal>
	</M.Root>
);

export const MenuItem = ({
	className,
	destructive,
	...props
}: ComponentProps<typeof M.Item> & { destructive?: boolean }) => (
	<M.Item
		className={cn(ITEM, destructive && "text-destructive", className)}
		{...props}
	/>
);

export const MenuSeparator = () => (
	<M.Separator className="-mx-1 my-1 h-px bg-border" />
);

/** One of a few values, picked from a submenu that names the current one. */
export function MenuChoice<V extends string>({
	label,
	value,
	options,
	onChange,
	disabled,
}: {
	label: string;
	value: V;
	options: { value: V; label: string; disabled?: boolean }[];
	onChange: (value: V) => void;
	disabled?: boolean;
}) {
	const current = options.find((o) => o.value === value)?.label;
	return (
		<M.Sub>
			<M.SubTrigger className={ITEM} disabled={disabled}>
				{label}
				<span className="ml-auto pl-4 text-muted-foreground">{current}</span>
				<ChevronRight className="text-muted-foreground" />
			</M.SubTrigger>
			<M.Portal>
				<M.SubContent sideOffset={4} className={CONTENT}>
					<M.RadioGroup value={value} onValueChange={(v) => onChange(v as V)}>
						{options.map((o) => (
							<M.RadioItem
								key={o.value}
								value={o.value}
								disabled={o.disabled}
								className={cn(ITEM, "pl-8")}
							>
								<M.ItemIndicator className="absolute left-2 flex items-center">
									<Check />
								</M.ItemIndicator>
								{o.label}
							</M.RadioItem>
						))}
					</M.RadioGroup>
				</M.SubContent>
			</M.Portal>
		</M.Sub>
	);
}

/** An on/off setting kept in the menu: ticked while on. The menu stays open on a tap. */
export const MenuCheck: FC<{
	checked: boolean;
	onChange: (checked: boolean) => void;
	disabled?: boolean;
	children: ReactNode;
}> = ({ checked, onChange, disabled, children }) => (
	<M.CheckboxItem
		checked={checked}
		disabled={disabled}
		onCheckedChange={(v) => onChange(v === true)}
		onSelect={(e) => e.preventDefault()}
		className={cn(ITEM, "pl-8")}
	>
		<M.ItemIndicator className="absolute left-2 flex items-center">
			<Check />
		</M.ItemIndicator>
		{children}
	</M.CheckboxItem>
);
