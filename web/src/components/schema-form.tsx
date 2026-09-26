// A plugin's JSON Schema as a form: the settings dialog (`/__config`) and a plugin's tab on a
// library entry's page (`/__game`) both render with it. Anything the form can't express falls back
// to a JSON editor; the plugin validates by decode on save either way.
import { type FC, useState } from "react";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";
import { m } from "@/paraglide/messages";

export type JsonObject = Record<string, unknown>;

export interface JsonSchemaNode {
	type?: string;
	title?: string;
	description?: string;
	default?: unknown;
	enum?: string[];
	/** `pf:path` / `pf:path:write`: a folder the console grants the plugin on save. */
	format?: string;
	properties?: Record<string, JsonSchemaNode>;
	items?: JsonSchemaNode;
	allOf?: JsonSchemaNode[];
}

export interface JsonSchemaDoc {
	schema?: JsonSchemaNode;
}

/** A checked schema (effect's `Schema.Int`, `.check(...)`) nests its annotations under `allOf`. */
const flatten = (node: JsonSchemaNode): JsonSchemaNode =>
	(node.allOf ?? []).reduce<JsonSchemaNode>(
		(acc, branch) => Object.assign(acc, branch),
		{ ...node },
	);

/** Can this field be a real input? One that can't sends the whole form to the JSON editor. */
const renderable = (node: JsonSchemaNode): boolean => {
	const n = flatten(node);
	if (n.enum) return true;
	if (n.type === "boolean" || n.type === "string") return true;
	if (n.type === "number" || n.type === "integer") return true;
	if (n.type === "array" && flatten(n.items ?? {}).type === "string")
		return true;
	if (n.type === "object" && n.properties) {
		return Object.values(n.properties).every(renderable);
	}
	return false;
};

const handed = (n: JsonSchemaNode): "read" | "write" | null =>
	n.format === "pf:path:write"
		? "write"
		: n.format === "pf:path"
			? "read"
			: null;

/** Explorer's "Copy as path" wraps a path in double quotes; the path itself has none. */
export const unquote = (s: string): string => s.replace(/^\s*"(.*)"\s*$/, "$1");

/** One entry per line: edge space and blank lines dropped, handed paths unquoted. */
export const toLines = (text: string, paths: boolean): string[] =>
	text
		.split("\n")
		.map((s) => (paths ? unquote(s) : s).trim())
		.filter((s) => s !== "");

/**
 * The form. `onChange` gets `null` while the JSON editor holds text that is not an object, so
 * the caller can hold its Save. `grantee` names who receives a handed folder.
 */
export const SchemaForm: FC<{
	schema: JsonSchemaDoc | null;
	value: JsonObject;
	onChange: (value: JsonObject | null) => void;
	grantee: string;
}> = ({ schema, value, onChange, grantee }) => {
	const [text, setText] = useState(() => JSON.stringify(value, null, 2));
	const root = schema?.schema ? flatten(schema.schema) : undefined;
	const props = root?.properties;
	// Partial rendering would hide a setting the operator then cannot change: all or nothing.
	if (props === undefined || !Object.values(props).every(renderable)) {
		return (
			<div className="space-y-3">
				<p className="text-xs text-muted-foreground">
					{m.plugin_form_json_hint()}
				</p>
				<Textarea
					className="h-64 font-mono text-xs"
					value={text}
					spellCheck={false}
					onChange={(e) => {
						setText(e.target.value);
						try {
							const parsed = JSON.parse(e.target.value) as unknown;
							const ok =
								parsed !== null &&
								typeof parsed === "object" &&
								!Array.isArray(parsed);
							onChange(ok ? (parsed as JsonObject) : null);
						} catch {
							onChange(null);
						}
					}}
				/>
			</div>
		);
	}
	return (
		<div className="space-y-4">
			{Object.entries(props).map(([key, node]) => (
				<Field
					key={key}
					name={key}
					node={flatten(node)}
					value={value[key]}
					grantee={grantee}
					onChange={(v) => onChange({ ...value, [key]: v })}
				/>
			))}
		</div>
	);
};

const Help: FC<{
	node: JsonSchemaNode;
	grantee: string;
	access: "read" | "write" | null;
}> = ({ node, grantee, access }) => (
	<>
		{node.description && (
			<p className="text-xs text-muted-foreground">{node.description}</p>
		)}
		{access && (
			<p className="text-xs text-muted-foreground">
				{access === "write"
					? m.plugin_form_path_write({ plugin: grantee })
					: m.plugin_form_path_read({ plugin: grantee })}
			</p>
		)}
	</>
);

/** One schema field. `undefined` in the value means "unset" — the file keeps its default out. */
const Field: FC<{
	name: string;
	node: JsonSchemaNode;
	value: unknown;
	grantee: string;
	onChange: (v: unknown) => void;
}> = ({ name, node, value, grantee, onChange }) => {
	const label = node.title ?? name;
	const id = `cfg-${name}`;

	if (node.type === "object" && node.properties) {
		const nested = (value ?? {}) as JsonObject;
		return (
			<fieldset className="space-y-3 rounded-lg border p-3">
				<legend className="px-1 text-sm font-medium">{label}</legend>
				{Object.entries(node.properties).map(([k, n]) => (
					<Field
						key={k}
						name={`${name}.${k}`}
						node={flatten(n)}
						value={nested[k]}
						grantee={grantee}
						onChange={(v) => onChange({ ...nested, [k]: v })}
					/>
				))}
			</fieldset>
		);
	}

	if (node.enum) {
		return (
			<div className="space-y-1">
				<Label htmlFor={id}>{label}</Label>
				<Select
					value={String(value ?? node.default ?? node.enum[0])}
					onValueChange={onChange}
				>
					<SelectTrigger id={id} size="sm">
						<SelectValue />
					</SelectTrigger>
					<SelectContent>
						{node.enum.map((opt) => (
							<SelectItem key={opt} value={opt}>
								{opt}
							</SelectItem>
						))}
					</SelectContent>
				</Select>
				<Help node={node} grantee={grantee} access={null} />
			</div>
		);
	}

	if (node.type === "boolean") {
		const checked = (value ?? node.default ?? false) as boolean;
		return (
			<div className="space-y-1">
				<div className="flex items-center gap-2">
					<Checkbox
						id={id}
						checked={checked}
						onCheckedChange={(next) => onChange(next === true)}
					/>
					<Label htmlFor={id}>{label}</Label>
				</div>
				<Help node={node} grantee={grantee} access={null} />
			</div>
		);
	}

	if (node.type === "array") {
		const access = handed(flatten(node.items ?? {}));
		return (
			<div className="space-y-1">
				<Label htmlFor={id}>{label}</Label>
				<LinesField
					id={id}
					list={(value ?? node.default ?? []) as string[]}
					paths={access !== null}
					onChange={onChange}
				/>
				<Help node={node} grantee={grantee} access={access} />
			</div>
		);
	}

	const numeric = node.type === "number" || node.type === "integer";
	const access = handed(node);
	return (
		<div className="space-y-1">
			<Label htmlFor={id}>{label}</Label>
			<Input
				id={id}
				type={numeric ? "number" : "text"}
				className={access ? "font-mono text-xs" : undefined}
				value={String(value ?? "")}
				placeholder={node.default != null ? String(node.default) : undefined}
				onChange={(e) => {
					const v = e.target.value;
					// Emptied means unset, not zero or "": the file keeps a value never chosen out.
					if (v === "") return onChange(undefined);
					onChange(numeric ? Number(v) : access ? unquote(v) : v);
				}}
			/>
			<Help node={node} grantee={grantee} access={access} />
		</div>
	);
};

/** The shape every "extra folders" setting wants. The text stays as typed, so a space or a new
 * line survives the keystroke; blur shows what will be saved. */
const LinesField: FC<{
	id: string;
	list: string[];
	paths: boolean;
	onChange: (v: string[]) => void;
}> = ({ id, list, paths, onChange }) => {
	const [text, setText] = useState(() => list.join("\n"));
	return (
		<Textarea
			id={id}
			className="h-24 font-mono text-xs"
			value={text}
			spellCheck={!paths}
			onChange={(e) => {
				setText(e.target.value);
				onChange(toLines(e.target.value, paths));
			}}
			onBlur={() => setText(list.join("\n"))}
		/>
	);
};
