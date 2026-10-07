import { motion } from "motion/react";
import * as React from "react";
import { cn } from "@/lib/utils";

const Table = React.forwardRef<
	HTMLTableElement,
	React.HTMLAttributes<HTMLTableElement>
>(({ className, ...props }, ref) => (
	<div className="relative w-full overflow-auto">
		<table
			ref={ref}
			className={cn("w-full caption-bottom text-sm", className)}
			{...props}
		/>
	</div>
));
Table.displayName = "Table";

const TableHeader = React.forwardRef<
	HTMLTableSectionElement,
	React.HTMLAttributes<HTMLTableSectionElement>
>(({ className, ...props }, ref) => (
	<thead ref={ref} className={cn("[&_tr]:border-b", className)} {...props} />
));
TableHeader.displayName = "TableHeader";

const TableBody = React.forwardRef<
	HTMLTableSectionElement,
	React.HTMLAttributes<HTMLTableSectionElement>
>(({ className, ...props }, ref) => (
	<tbody
		ref={ref}
		className={cn("[&_tr:last-child]:border-0", className)}
		{...props}
	/>
));
TableBody.displayName = "TableBody";

const TableRow = React.forwardRef<
	HTMLTableRowElement,
	React.HTMLAttributes<HTMLTableRowElement>
>(({ className, ...props }, ref) => (
	<tr
		ref={ref}
		className={cn(
			"border-b transition-colors hover:bg-muted/50 data-[state=selected]:bg-muted",
			className,
		)}
		{...props}
	/>
));
TableRow.displayName = "TableRow";

const TableHead = React.forwardRef<
	HTMLTableCellElement,
	React.ThHTMLAttributes<HTMLTableCellElement>
>(({ className, ...props }, ref) => (
	<th
		ref={ref}
		className={cn(
			"h-10 px-card text-left align-middle font-medium text-muted-foreground [&:has([role=checkbox])]:pr-0",
			className,
		)}
		{...props}
	/>
));
TableHead.displayName = "TableHead";

const TableCell = React.forwardRef<
	HTMLTableCellElement,
	React.TdHTMLAttributes<HTMLTableCellElement>
>(({ className, ...props }, ref) => (
	<td
		ref={ref}
		className={cn(
			"p-card py-2 align-middle [&:has([role=checkbox])]:pr-0",
			className,
		)}
		{...props}
	/>
));
TableCell.displayName = "TableCell";

/**
 * A table on a phone is a list: a column only a wide screen has room for takes `WIDE`, and its
 * value rides the first cell's `RowDetails` line instead. Nothing scrolls sideways.
 */
const WIDE = "hidden md:table-cell";

/** The details line under a row's first cell, below `md`: what the `WIDE` columns carry. */
const RowDetails = ({ children }: { children: React.ReactNode }) => (
	<span className="block truncate text-xs font-normal text-muted-foreground md:hidden">
		{children}
	</span>
);

/** For a body whose rows stagger in: `staggerProps` on the body, `ROW` on each row. */
const MotionTableBody = motion.create(TableBody);
const MotionTableRow = motion.create(TableRow);

export {
	MotionTableBody,
	MotionTableRow,
	RowDetails,
	Table,
	TableBody,
	TableCell,
	TableHead,
	TableHeader,
	TableRow,
	WIDE,
};
