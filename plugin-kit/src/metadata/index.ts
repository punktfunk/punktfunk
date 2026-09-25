// `@punktfunk/plugin-kit/metadata` — the framework for Art & Metadata sources: plugins that fill
// art and details for library entries other plugins list. See planning
// `design/metadata-sources.md`.
export {
	type Candidate,
	defineMetadataPlugin,
	type Image,
	type MetadataPlugin,
	type MetadataPluginDef,
	type MetadataStatus,
	SourceRateLimited,
	SourceUnauthorized,
} from "./define.js";
export {
	type Found,
	HIT_TTL_MS,
	identityOf,
	isFresh,
	isHttpUrl,
	type LibraryEntry,
	type Match,
	MISS_TTL_MS,
	type Offers,
	ownView,
	toRow,
	Verdict,
	wanted,
} from "./rules.js";
