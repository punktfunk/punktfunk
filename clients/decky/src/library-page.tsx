// Punktfunk on Steam's own game page.
//
// Steam Remote Play turns a title's Play button into "Stream" when another of your Steam
// clients has it installed, and the ▾ beside Play lists those clients. Punktfunk hosts belong
// in that same list (see play-from.tsx). Getting there means rendering through Steam's page:
// the `/library/app/:appid` route is patched, and from its render output the descent below
// walks the section components down to the play bar row, where the Play button lives.
import { RoutePatch, routerHook } from "@decky/api";
import {
  afterPatch,
  appDetailsClasses,
  basicAppDetailsSectionStylerClasses,
  createReactTreePatcher,
  findInReactTree,
  Navigation,
} from "@decky/ui";
import { ReactElement } from "react";
import { hostsForApp } from "./hosts/catalog";
import { Game } from "./game";
import { getHostStore, refreshHostsIfStale } from "./hosts/store";
import { diag, verbose } from "./lib/diag";
import { read, write } from "./lib/storage";
import { collectElements, createRenderPatcher, describe } from "./patch";
import { patchPlayGroup, resetPlayFrom } from "./play-from";
import { steamAppIdForShortcut } from "./steam/keys";

const ROUTE = "/library/app/:appid";
const STYLE_KEY = "punktfunk-style";

// On by default. The one preference the plugin keeps for itself: it is about the plugin's own
// footprint in Steam's UI, not about streaming, so it has no home in the client's settings.
const PREF_KEY = "punktfunk:gamePageStream";

export function gamePageStreamEnabled(): boolean {
  return read(PREF_KEY) !== "0";
}

export function setGamePageStreamEnabled(on: boolean): void {
  write(PREF_KEY, on ? "1" : "0");
}

/** Steam's `app_type` for a non-Steam shortcut — never a host's `steam:<appid>`. */
const APP_TYPE_SHORTCUT = 1073741824;

/** A game page opened this long after the last scan rescans in the background, so a title
 *  installed on the host since then shows up in the ▾ menu without a trip to the panel. */
const STALE_MS = 60_000;

// Steam shows the page of the app it launched or just closed. For a stream that is the hidden
// per-game shortcut, whose page is nothing a user should see; the place to be is the Steam
// title's own page. Back first — the title's page is normally what lies beneath — and only if
// that landed elsewhere, navigate to it. Steam shows one page at a time, so throttling the most
// recent shortcut is enough to stop a stuck stack spinning.
const REDIRECT_THROTTLE_MS = 1500;
let lastRedirect = { shortcutAppId: 0, at: 0 };
function redirectShortcutPage(shortcutAppId: number, steamAppId: number): void {
  const now = Date.now();
  if (lastRedirect.shortcutAppId === shortcutAppId && now - lastRedirect.at < REDIRECT_THROTTLE_MS) {
    return;
  }
  lastRedirect = { shortcutAppId, at: now };
  diag(`game page: shortcut ${shortcutAppId} stands for ${steamAppId} — returning to its page`);
  const target = `/library/app/${steamAppId}`;
  setTimeout(() => {
    try {
      Navigation.NavigateBack();
      setTimeout(() => {
        if (!window.location.pathname.endsWith(target)) {
          Navigation.Navigate(target);
        }
      }, 350);
    } catch (e) {
      diag(`game page: redirect failed: ${e}`);
    }
  }, 0);
}

// Steam's Play button while a Punktfunk host is chosen (play-from.tsx adds the class): Steam's
// gray at rest, our brand violet under focus or hover where Steam's is green. `!important`
// because Steam styles the element through `:enabled` selectors. The lens mark's own violets
// vanish on that fill, so those states swap it for the light pair. Rides the group as a 0×0 element.
const STYLE = `
  .punktfunk-play.gpfocus,
  .punktfunk-play:focus,
  .punktfunk-play:hover {
    --pf-back: #ffffff;
    --pf-deep: #cec9fb;
    background: linear-gradient(to right, #8c7ef5 0%, #5b4ce0 60%) 0% center / 330% 100% !important;
    color: #ffffff !important;
  }
`;

interface OverviewLike {
  appid?: unknown;
  app_type?: unknown;
  display_name?: unknown;
  icon_hash?: unknown;
}

type InnerContainer = ReactElement<{ children: any[]; className?: string }>;

// ---- Into the play bar --------------------------------------------------------------------
//
// Measured on a Deck: from the InnerContainer, the play section is a Focusable whose child
// (props `overview` + `onGameInfoButtonToggle`) renders a chain of "section" components (props
// `setSections` + `overview`), several of which come in identical-looking pairs, until one
// renders the play bar row: [Play group, stats, button group]. The button group is the
// Focusable with the AppButtons class holding the controller and settings buttons; the Play
// group holds Steam's Play button and its ▾.
//
// Every section-shaped child is patched (one wrap per component type) and the shared handler
// either recognises the row by that group or keeps descending. A branch that never reaches the
// row costs one tree walk.

/** The title of the page being rendered; set by the route handler before the descent runs. */
let currentGame: Game | null = null;
let reachedRow = false;
let rowTimer: ReturnType<typeof setTimeout> | null = null;

const isPlaySection = (x: any): boolean => !!x?.props && "onGameInfoButtonToggle" in x.props && "overview" in x.props;
const isSectionShaped = (x: any): boolean => !!x?.props && "setSections" in x.props && "overview" in x.props;

function onSectionRender(out: any): any {
  if (!out || !currentGame) {
    return out;
  }
  const group = findInReactTree(
    out,
    (x: any) =>
      Array.isArray(x?.props?.children) &&
      typeof x?.props?.className === "string" &&
      x.props.className.includes(basicAppDetailsSectionStylerClasses.AppButtons),
  ) as InnerContainer | undefined;
  if (group) {
    const kids = group.props.children;
    if (!kids.some((c) => c?.key === STYLE_KEY)) {
      kids.splice(0, 0, <style key={STYLE_KEY}>{STYLE}</style>);
    }
    if (!reachedRow) {
      reachedRow = true;
      diag(
        `game page ${currentGame.appId}: play bar reached, ` +
          `${hostsForApp(currentGame.appId, getHostStore().views).length} host(s) have it`,
      );
    }
    // The same row holds the Play group: its ▾ menu and Play button get Punktfunk's hosts.
    patchPlayGroup(out);
    return out;
  }
  const next = collectElements(out, isSectionShaped);
  if (verbose()) {
    diag(`deep: ${describe(out)} → ${next.length} section child(ren): ${next.map(describe).join(", ")}`);
  }
  if (next.length === 0 && !reachedRow) {
    // A dead end: say what this output's Focusables are called, so a moved class is visible.
    const classes = collectElements(out, (x: any) => typeof x?.props?.className === "string")
      .map((x: any) => String(x.props.className).split(" ")[0])
      .slice(0, 6);
    diag(`deep: no play bar in [${classes.join(" ")}] (want ${basicAppDetailsSectionStylerClasses.AppButtons})`);
  }
  for (const child of next) {
    sections.patch(child);
  }
  return out;
}

const sections = createRenderPatcher(onSectionRender, "sections");

/** Start the descent from the InnerContainer's render. False when the play section itself is
 *  missing from the page. */
function reachPlayBar(ret: ReactElement, game: Game): boolean {
  const section = findInReactTree(ret, isPlaySection);
  if (!section) {
    return false;
  }
  // A new title starts the watch over: the row has to be found on THIS page, and Steam's tree
  // can move under a client update mid-session.
  if (currentGame?.appId !== game.appId) {
    reachedRow = false;
    clearRowTimer();
  }
  currentGame = game;
  sections.patch(section);
  if (!reachedRow && !rowTimer) {
    // The row only shows up once the children have rendered; a few seconds without it means
    // Steam's tree has moved, and the trace above says where the walk stopped.
    rowTimer = setTimeout(() => {
      rowTimer = null;
      if (!reachedRow) {
        diag("game page: the play bar was not reached — Steam's page tree has changed");
      }
    }, 4000);
  }
  return true;
}

function clearRowTimer(): void {
  if (rowTimer) {
    clearTimeout(rowTimer);
    rowTimer = null;
  }
}

/**
 * Patch the game page: find the route's render function, and after each render start the
 * descent to the play bar. Every lookup is defensive — Steam's tree is not an API, and a miss
 * must leave the page exactly as Steam drew it.
 */
function patchLibraryApp(): RoutePatch {
  return routerHook.addPatch(ROUTE, (tree: any) => {
    const routeProps = findInReactTree(tree, (x: any) => x?.renderFunc);
    if (!routeProps) {
      diag("game page: no renderFunc in route tree");
      return tree;
    }
    let overview: OverviewLike | undefined;
    const handler = createReactTreePatcher(
      [
        (node: any) => {
          const children = findInReactTree(node, (x: any) => x?.props?.children?.props?.overview)
            ?.props?.children;
          if (typeof children !== "object" || typeof children?.props?.overview !== "object") {
            diag("game page: no child carrying an overview");
            return null;
          }
          overview = children.props.overview as OverviewLike;
          return children;
        },
      ],
      (_: unknown[], ret?: ReactElement) => {
        // Guarded: an exception in a route patch is Steam's error screen for the whole page.
        try {
          return placeOnPage(ret);
        } catch (e) {
          diag(`game page: handler failed: ${e}`);
          return ret;
        }
      },
      "punktfunk-stream",
    );
    afterPatch(routeProps, "renderFunc", handler);

    function placeOnPage(ret?: ReactElement): ReactElement | undefined {
      if (!ret || !gamePageStreamEnabled()) {
        diag(`game page: ${ret ? "disabled by preference" : "empty render"}`);
        return ret;
      }
      const appId = overview?.appid;
      if (typeof appId === "number" && overview?.app_type === APP_TYPE_SHORTCUT) {
        const steamAppId = steamAppIdForShortcut(appId);
        if (steamAppId != null) {
          redirectShortcutPage(appId, steamAppId);
        }
        return ret;
      }
      if (
        typeof appId !== "number" ||
        overview?.app_type === APP_TYPE_SHORTCUT ||
        typeof overview?.display_name !== "string"
      ) {
        diag(`game page: skipped overview appid=${String(appId)} type=${String(overview?.app_type)}`);
        return ret;
      }
      if (!findInReactTree(ret, (x: any) => !!x?.props?.className?.includes?.(appDetailsClasses.InnerContainer))) {
        diag(`game page ${appId}: no InnerContainer (${appDetailsClasses.InnerContainer})`);
        return ret;
      }
      const game: Game = {
        appId,
        title: overview.display_name,
        iconHash: typeof overview.icon_hash === "string" ? overview.icon_hash : "",
      };
      if (!reachPlayBar(ret, game)) {
        diag(`game page ${appId}: no play section in the page`);
      }
      void refreshHostsIfStale(STALE_MS);
      return ret;
    }
    return tree;
  });
}

/** Install the game-page patch. Returns the remover for `onDismount`. */
export function installGamePageStream(): () => void {
  const patch = patchLibraryApp();
  return () => {
    routerHook.removePatch(ROUTE, patch);
    sections.reset();
    resetPlayFrom();
    clearRowTimer();
    reachedRow = false;
    currentGame = null;
  };
}
