import { useQueryClient } from "@tanstack/react-query";
import {
	getGetDownloadsQueryKey,
	getGetLibraryPageQueryKey,
	getGetLibraryQueryKey,
	useCancelLibraryInstall,
	useGetDownloads,
	useInstallLibraryEntry,
	usePauseLibraryInstall,
	useUninstallLibraryEntry,
} from "@/api/gen/library/library";
import type { Download } from "@/api/gen/model/download";

/** Speed and time left move every second while something downloads. */
const POLL_MS = 1_000;

/** Making progress, or expected to. */
export const isLive = (d: Download) =>
	d.state === "queued" || d.state === "downloading" || d.state === "installing";

/** Every download, polled while one is live; `downloads.changed` refreshes it otherwise. */
export const useDownloads = () =>
	useGetDownloads({
		query: {
			refetchInterval: (q) =>
				(q.state.data ?? []).some(isLive) ? POLL_MS : false,
		},
	});

/** Install, pause, cancel and remove; each refreshes the list and the library when it lands. */
export const useInstallActions = () => {
	const qc = useQueryClient();
	const mutation = {
		onSettled: () => {
			void qc.invalidateQueries({ queryKey: getGetDownloadsQueryKey() });
			void qc.invalidateQueries({ queryKey: getGetLibraryQueryKey() });
			void qc.invalidateQueries({ queryKey: getGetLibraryPageQueryKey() });
		},
	};
	return {
		install: useInstallLibraryEntry({ mutation }),
		pause: usePauseLibraryInstall({ mutation }),
		cancel: useCancelLibraryInstall({ mutation }),
		uninstall: useUninstallLibraryEntry({ mutation }),
	};
};
