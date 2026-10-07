// A row's actions: buttons in the row on a wide screen, folded into ⋯ on a phone. Radix's
// dropdown menu in the console's tokens, like the Select wrapper.
import { Check, ChevronRight, MoreHorizontal } from "lucide-react";
import { DropdownMenu as M } from "radix-ui";
import { type ComponentProps, type FC, Fragment, type ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";

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

type Choice = { value: string; label: string; disabled?: boolean };

/** One thing a row can do: a button, an on/off setting, or one of a few values. */
export type RowAction =
	| {
			kind?: "action";
			label: string;
			onSelect: () => void;
			icon?: ReactNode;
			/** In the row as its icon alone, named by its tooltip: for a dense row. */
			iconOnly?: boolean;
			destructive?: boolean;
			disabled?: boolean;
	  }
	| {
			kind: "check";
			label: string;
			checked: boolean;
			onChange: (checked: boolean) => void;
			hint?: string;
			disabled?: boolean;
	  }
	| {
			kind: "choice";
			label: string;
			value: string;
			options: Choice[];
			onChange: (value: string) => void;
			disabled?: boolean;
	  };

/**
 * A row's actions, listed once: buttons in the row from `md` up, the same list in ⋯ below it. A
 * lone action stays a button at every width. Falsy entries are skipped, so a caller writes each
 * action's condition in place. A destructive action gets a separator above it in the menu.
 * `labelsFrom="xl"` shows an action's icon alone until `xl`, for a row with four of them.
 */
export const RowActions: FC<{
	actions: (RowAction | false | null | undefined | "")[];
	label?: string;
	disabled?: boolean;
	labelsFrom?: "xl";
}> = ({ actions, label = m.common_more_actions(), disabled, labelsFrom }) => {
	const list = actions.filter((a): a is RowAction => !!a);
	if (list.length === 0) return null;
	const inline = list.map((a) => (
		<InlineAction
			key={a.label}
			action={a}
			disabled={disabled}
			short={labelsFrom === "xl"}
		/>
	));
	if (list.length === 1)
		return <div className="flex items-center gap-1">{inline}</div>;
	const danger = list.findIndex((a) => isAction(a) && a.destructive);
	return (
		<>
			<div className="hidden items-center gap-1 md:flex">{inline}</div>
			<div className="md:hidden">
				<RowMenu label={label} disabled={disabled}>
					{list.map((a, i) => (
						<Fragment key={a.label}>
							{i === danger && i > 0 && <MenuSeparator />}
							<MenuAction action={a} />
						</Fragment>
					))}
				</RowMenu>
			</div>
		</>
	);
};

const isAction = (
	a: RowAction,
): a is Extract<RowAction, { onSelect: () => void }> =>
	(a.kind ?? "action") === "action";

const InlineAction: FC<{
	action: RowAction;
	disabled?: boolean;
	/** The label shows from `xl`; the icon carries it below. */
	short?: boolean;
}> = ({ action: a, disabled, short }) => {
	if (a.kind === "check")
		return (
			// biome-ignore lint/a11y/noLabelWithoutControl: the Checkbox inside is the control.
			<label className="flex items-center gap-2 px-2 text-sm" title={a.hint}>
				<Checkbox
					checked={a.checked}
					disabled={disabled || a.disabled}
					onCheckedChange={(v) => a.onChange(v === true)}
				/>
				{a.label}
			</label>
		);
	if (a.kind === "choice")
		return (
			<Select
				value={a.value}
				disabled={disabled || a.disabled}
				onValueChange={a.onChange}
			>
				<SelectTrigger
					aria-label={a.label}
					title={a.label}
					className="h-8 w-auto gap-1 text-sm"
				>
					<SelectValue />
				</SelectTrigger>
				<SelectContent>
					{a.options.map((o) => (
						<SelectItem key={o.value} value={o.value} disabled={o.disabled}>
							{o.label}
						</SelectItem>
					))}
				</SelectContent>
			</Select>
		);
	const named = a.iconOnly || (short && a.icon);
	return (
		<Button
			variant="ghost"
			size={a.iconOnly ? "icon" : "sm"}
			aria-label={named ? a.label : undefined}
			title={named ? a.label : undefined}
			disabled={disabled || a.disabled}
			className={cn(a.destructive && "text-destructive hover:text-destructive")}
			onClick={a.onSelect}
		>
			{a.icon}
			{!a.iconOnly &&
				(short && a.icon ? (
					<span className="hidden xl:inline">{a.label}</span>
				) : (
					a.label
				))}
		</Button>
	);
};

const MenuAction: FC<{ action: RowAction }> = ({ action: a }) => {
	if (a.kind === "check")
		return (
			<MenuCheck
				checked={a.checked}
				disabled={a.disabled}
				onChange={a.onChange}
			>
				<span title={a.hint}>{a.label}</span>
			</MenuCheck>
		);
	if (a.kind === "choice")
		return (
			<MenuChoice
				label={a.label}
				value={a.value}
				options={a.options}
				disabled={a.disabled}
				onChange={a.onChange}
			/>
		);
	return (
		<MenuItem
			destructive={a.destructive}
			disabled={a.disabled}
			onSelect={a.onSelect}
		>
			{a.icon}
			{a.label}
		</MenuItem>
	);
};
