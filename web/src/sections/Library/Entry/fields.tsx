import type { FC, ReactNode } from "react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";

/** One labelled input, or its value as text when the entry is read-only. An empty read-only
 * value renders nothing. */
export const TextField: FC<{
	id: string;
	label: string;
	value: string;
	onChange: (value: string) => void;
	readOnly?: boolean;
	help?: string;
	type?: string;
	required?: boolean;
	multiline?: boolean;
	placeholder?: string;
}> = ({
	id,
	label,
	value,
	onChange,
	readOnly,
	help,
	type,
	required,
	multiline,
	placeholder,
}) => {
	if (readOnly) return <ReadRow label={label} value={value} />;
	const inputId = `entry-${id}`;
	return (
		<div className="space-y-2">
			<Label htmlFor={inputId}>{label}</Label>
			{multiline ? (
				<Textarea
					id={inputId}
					rows={4}
					value={value}
					placeholder={placeholder}
					onChange={(e) => onChange(e.target.value)}
				/>
			) : (
				<Input
					id={inputId}
					type={type}
					inputMode={
						type === "url" ? "url" : type === "number" ? "numeric" : undefined
					}
					required={required}
					value={value}
					placeholder={placeholder}
					onChange={(e) => onChange(e.target.value)}
				/>
			)}
			{help && <p className="text-xs text-muted-foreground">{help}</p>}
		</div>
	);
};

/** A label over a value, for read-only entries. Nothing when there is no value. */
export const ReadRow: FC<{ label: string; value: ReactNode }> = ({
	label,
	value,
}) =>
	value === "" || value == null ? null : (
		<div className="min-w-0 space-y-1">
			<p className="text-xs font-medium text-muted-foreground">{label}</p>
			<div className="break-words text-sm">{value}</div>
		</div>
	);

/** One group of a tab, as a card. Untitled when it is the tab's whole content. */
export const Group: FC<{
	title?: string;
	help?: string;
	children: ReactNode;
}> = ({ title, help, children }) => (
	<Card>
		{title && (
			<CardHeader>
				<CardTitle>{title}</CardTitle>
				{help && <p className="text-sm text-muted-foreground">{help}</p>}
			</CardHeader>
		)}
		<CardContent className="@container space-y-4">{children}</CardContent>
	</Card>
);
