// A title of this device's own library as a Steam entry of its own: visible, named after the
// title, so it runs in Steam's tree with its own name, art and Steam Input layout. These are not
// streams: running.ts never takes them, and no Punktfunk layout is written for their names.
import type { LocalEntry } from "../backend";
import { localArtKey, localKey } from "./keys";
import { localLaunchOptions } from "./launch-options";
import { type Ensured, ensureShortcut } from "./shortcuts";

/** Ensure the title's entry: its launch options name only this device's host binary. The sync
 *  dresses it after, one entry at a time. */
export function ensureLocalEntry(entry: LocalEntry, hostBin: string): Promise<Ensured> {
  return ensureShortcut({
    key: localKey(entry.id),
    name: entry.title,
    configMarker: null,
    launchOptions: (runner) => localLaunchOptions(runner, hostBin),
    artKey: localArtKey,
    dress: () => {},
  });
}
