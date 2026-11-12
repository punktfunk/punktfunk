import { toaster } from "@decky/api";

/** A Decky toast under the "Punktfunk" title, unless `opts.title` names another. */
export function toast(body: string, opts: { title?: string; duration?: number } = {}): void {
  toaster.toast({ title: "Punktfunk", body, ...opts });
}
