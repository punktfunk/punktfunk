// @punktfunk/plugin-kit — Effect-based framework for punktfunk plugins.

export {
	type AccessRequestOutcome,
	type AccessRequestPath,
	requestAccess,
	requestCores,
	requestEmulators,
	unreachable,
} from "./access.js";
export { type CacheStore, makeCacheStore } from "./cache-store.js";
export { type CliCommand, runPluginCli } from "./cli.js";
export { type ConfigService, makeConfigService } from "./config.js";
export {
	type DownloadReporter,
	type DownloadRow,
	type DownloadState,
	downloadReporter,
	type InstallAction,
	type InstallAsk,
	InstallRefused,
	makeDownloadReporter,
	makeInstallHandler,
	NotMyTitle,
	type ServeUiInstall,
} from "./downloads.js";
export * from "./errors.js";
export {
	HostClient,
	type HostClientService,
	hostClientFromFacade,
	PluginInfo,
	type PluginInfoService,
	pluginInfoLayer,
} from "./host-client.js";
export { loggingLayer } from "./logging.js";
export {
	atomicWriteFile,
	ensureStateDir,
	pluginIngestDir,
	pluginStateDir,
	statePath,
} from "./paths.js";
export {
	ART_KINDS,
	ArtKind,
	Artwork,
	AudioPolicy,
	DEFAULT_RUNNING_TTL_S,
	DetectHint,
	EntryIds,
	GameMeta,
	LaunchSpec,
	MetadataEntry,
	type MetaField,
	OnWindow,
	PrepStep,
	ProviderClient,
	type ProviderClientService,
	ProviderEntry,
	type RunningAccepted,
	type RunningTitle,
} from "./reconcile.js";
export {
	definePluginKit,
	type PluginKitDef,
	runPluginKitDirect,
} from "./runtime.js";
export { type SseRouteOptions, sseRoute } from "./sse.js";
export {
	DEFAULT_FS_CHANGE_MIN_INTERVAL,
	type LastSync,
	makeSyncEngine,
	type SyncEngine,
	type SyncEngineOptions,
	type SyncOutcome,
	type SyncReason,
	type SyncSettings,
	type SyncStatus,
} from "./sync-engine.js";
export {
	deriveConfigJsonSchema,
	handedPath,
	httpApiEnv,
	makeConfigHandler,
	makeGameHandler,
	makeHoldHandler,
	type ServeUiConfig,
	type ServeUiGame,
	type ServeUiHolds,
	type ServeUiOptions,
	type StatusLine,
	serveUi,
} from "./ui-server.js";
export { Install } from "./wire.js";
