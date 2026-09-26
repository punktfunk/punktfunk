import type { FC } from "react";
import { m } from "@/paraglide/messages";
import { useSourceNames } from "../../Sources";
import { Group, TextField } from "../fields";
import type { FormState } from "../model";
import type { TabProps } from "./types";

const META: (keyof FormState)[] = [
	"description",
	"developer",
	"publisher",
	"releaseYear",
	"players",
	"platform",
	"region",
	"genres",
	"tags",
];

/** The `GameMeta` field a form key edits, as `filled` names it. */
const metaField = (key: keyof FormState): string =>
	key === "releaseYear" ? "release_year" : key;

/** Title and the `GameMeta` fields. A value an Art & Metadata source filled says where it came
 * from; on the operator's own entry it shows as a placeholder until they type their own. */
export const InformationTab: FC<TabProps> = ({
	draft,
	set,
	readOnly,
	entry,
}) => {
	const nameOf = useSourceNames();
	const borrowed = (key: keyof FormState) => {
		const source = entry?.filled?.[metaField(key)];
		if (source === undefined) return undefined;
		const raw = (entry as Record<string, unknown> | null)?.[metaField(key)];
		const value = Array.isArray(raw) ? raw.join(", ") : String(raw ?? "");
		return { source: nameOf(source) ?? source, value };
	};
	const field = (key: keyof FormState, label: string, help?: string) => {
		const from = borrowed(key);
		return {
			id: key,
			value: draft[key] as string,
			onChange: (v: string) => set(key, v),
			readOnly,
			label:
				readOnly && from
					? `${label} · ${m.library_field_from({ source: from.source })}`
					: label,
			help:
				!readOnly && from
					? m.library_field_from_editable({ source: from.source })
					: help,
			placeholder: !readOnly && from ? from.value : undefined,
		};
	};
	if (readOnly && META.every((k) => draft[k] === "")) {
		return (
			<Group title={m.library_entry_tab_information()}>
				<p className="text-sm text-muted-foreground">
					{m.library_entry_no_details()}
				</p>
			</Group>
		);
	}
	return (
		<Group title={m.library_entry_tab_information()}>
			{!readOnly && (
				<TextField {...field("title", m.library_field_title())} required />
			)}
			<TextField
				{...field("description", m.library_field_description())}
				multiline
			/>
			<div className="grid gap-4 @lg:grid-cols-2">
				<TextField {...field("developer", m.library_field_developer())} />
				<TextField {...field("publisher", m.library_field_publisher())} />
				{/* `type="number"` over a string: both are optional, and empty means unset. */}
				<TextField
					{...field("releaseYear", m.library_field_release_year())}
					type="number"
				/>
				<TextField
					{...field("players", m.library_field_players())}
					type="number"
				/>
				<TextField
					{...field(
						"platform",
						m.library_field_platform(),
						m.library_field_platform_help(),
					)}
				/>
				<TextField
					{...field(
						"region",
						m.library_field_region(),
						m.library_field_region_help(),
					)}
				/>
				<TextField
					{...field(
						"genres",
						m.library_field_genres(),
						m.library_field_genres_help(),
					)}
				/>
				<TextField
					{...field(
						"tags",
						m.library_field_tags(),
						m.library_field_tags_help(),
					)}
				/>
			</div>
		</Group>
	);
};
