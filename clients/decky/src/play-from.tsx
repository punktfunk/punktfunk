// Punktfunk hosts in Steam's own "Play from" dropdown, and Steam's Play button as Stream when
// one is chosen.
//
// The ▾ beside Play opens Steam's streaming selector: `overview.per_client_data` as "This device"
// / "Stream from: <PC>" rows, selection through `SteamClient.Apps.SetStreamingClientForApp`. That
// menu is built inside the Play button class's bound ShowStreamingMenu, so the ▾ is re-pointed
// at a menu built here from the same data with Steam's own Menu components, class names and
// localization tokens, plus a row per Punktfunk host that has the title. The choice is kept per
// title; while it stands, the Play button is re-dressed as Stream and launches ours.
//
// Steam draws the ▾ only while a second Steam client has the title, so where a Punktfunk host
// has it and Steam left the ▾ out, it is drawn here. No ▾, no re-dressed button: the menu's
// "This device" row is the only way back to Steam's Play.
import { findClassModule, Menu, MenuItem, MenuSeparator, showContextMenu } from "@decky/ui";
import { cloneElement, createElement } from "react";
import { FaCheck } from "react-icons/fa";
import { hostsForApp, subscribeCatalog } from "./catalog";
import { diag } from "./diag";
import { Game, PunktfunkMark, streamFrom } from "./game";
import { getHostStore, subscribeHosts } from "./hooks";
import type { HostView } from "./hosts";
import { collectElements, createRenderPatcher, describe } from "./patch";
import { isGameStreaming, stopGameStream, subscribeRunning } from "./steam";

declare const SteamClient: {
  Apps: { SetStreamingClientForApp(appId: number, clientId: string): void };
};

/** The overview fields the selector reads — Steam's object, so everything is optional. */
interface Overview {
  appid: number;
  display_name: string;
  icon_hash?: string;
  per_client_data?: ClientData[];
  selected_clientid?: string;
  BIsPerClientDataLocal?(c: ClientData): boolean;
  BIsSelectedClientLocal?(): boolean;
}

interface ClientData {
  clientid: string;
  client_name: string;
}

function gameOf(overview: Overview): Game {
  return {
    appId: overview.appid,
    title: overview.display_name,
    iconHash: typeof overview.icon_hash === "string" ? overview.icon_hash : "",
  };
}

// ---- The choice: which Punktfunk host this title's Play button streams from ------------------

const SEL_PREFIX = "punktfunk:playFrom:";

export function playFromSelection(appId: number): string | null {
  try {
    return localStorage.getItem(SEL_PREFIX + appId);
  } catch {
    return null;
  }
}

export function setPlayFromSelection(appId: number, ref: string | null): void {
  try {
    if (ref) {
      localStorage.setItem(SEL_PREFIX + appId, ref);
    } else {
      localStorage.removeItem(SEL_PREFIX + appId);
    }
  } catch {
    /* ignore */
  }
  rerenderPlayButtons();
}

/** The chosen host, if it is still one that can stream the title; otherwise Steam's own state
 *  stands and the choice is silently dropped from the picture. */
function selectedHost(appId: number, hosts: HostView[]): HostView | null {
  const ref = playFromSelection(appId);
  return ref ? (hosts.find((h) => h.ref === ref) ?? null) : null;
}

// The Play button instances on screen, so a choice, a scan or a stream ending redraws them.
const instances = new Set<any>();
let watching = false;

function rerenderPlayButtons(): void {
  for (const inst of instances) {
    // forceUpdate on an unmounted component is a silent no-op, never a throw, so the updater is
    // what says whether this one is still on screen. Without it the set grows for the session.
    if (inst.updater?.isMounted?.(inst) === false) {
      instances.delete(inst);
      continue;
    }
    try {
      inst.forceUpdate();
    } catch {
      instances.delete(inst);
    }
  }
}

function ensureWatching(): void {
  if (watching) {
    return;
  }
  watching = true;
  subscribeRunning(rerenderPlayButtons);
  subscribeCatalog(rerenderPlayButtons);
  subscribeHosts(rerenderPlayButtons);
}

// ---- Steam's words and classes --------------------------------------------------------------

/** Steam's own string for a token, with its `%1$s` / `%s` slot filled; English when the token
 *  is unknown to this client. */
function localize(token: string, fallback: string, arg?: string): string {
  let s = fallback;
  try {
    const lm = (window as any).LocalizationManager;
    const got = lm?.LocalizeString?.(token);
    if (typeof got === "string" && got && !got.startsWith("#")) {
      s = got;
    }
  } catch {
    /* fallback */
  }
  return arg == null ? s : s.replace(/%1\$s|%s/, arg);
}

interface MenuClasses {
  StreamingContextMenuItem?: string;
  CheckContainer?: string;
  StreamingTargetLabel?: string;
  StreamingSelector?: string;
  ButtonChild?: string;
  ShowingStreaming?: string;
}

let menuClasses: MenuClasses | null = null;
function classes(): MenuClasses {
  menuClasses ??= (findClassModule((m) => m.StreamingContextMenuItem) as MenuClasses | undefined) ?? {};
  return menuClasses;
}

// ---- The menu -------------------------------------------------------------------------------

/** Steam's "Play from" list, rebuilt from the same data, with our hosts after Steam's clients. */
export function openPlayFromMenu(overview: Overview, anchor?: EventTarget): void {
  const game = gameOf(overview);
  const hosts = hostsForApp(game.appId, getHostStore().views);
  const ours = selectedHost(game.appId, hosts);
  const cls = classes();
  const items = [];
  for (const client of overview.per_client_data ?? []) {
    const local = !!overview.BIsPerClientDataLocal?.(client);
    const selected =
      !ours &&
      (overview.selected_clientid === client.clientid || (local && !!overview.BIsSelectedClientLocal?.()));
    const label = local
      ? localize("#StreamingClient_Menu", "This device")
      : localize("#StreamingClient_StreamFrom", "Stream from: %s", client.client_name);
    items.push(
      <MenuItem
        key={`steam:${client.clientid}`}
        {...{ className: cls.StreamingContextMenuItem }}
        onSelected={() => {
          setPlayFromSelection(game.appId, null);
          SteamClient.Apps.SetStreamingClientForApp(game.appId, client.clientid);
        }}
      >
        <span className={cls.CheckContainer}>{selected && <FaCheck style={{ color: "#1a9fff" }} />}</span>
        <span className={cls.StreamingTargetLabel}>{label}</span>
      </MenuItem>,
    );
  }
  // Punktfunk hosts as a second group, drawn exactly like Steam's rows — same check column, same
  // label, Steam's text colour — with a small lens mark at the row's end as the only brand hint.
  // A separator is how Steam's own menu groups its entries.
  if (hosts.length > 0) {
    items.push(<MenuSeparator key="pf-sep" />);
  }
  for (const host of hosts) {
    const selected = ours?.ref === host.ref;
    items.push(
      <MenuItem
        key={`pf:${host.ref}`}
        {...{ className: cls.StreamingContextMenuItem }}
        onSelected={() => setPlayFromSelection(game.appId, host.ref)}
      >
        <span className={cls.CheckContainer}>{selected && <FaCheck style={{ color: "#1a9fff" }} />}</span>
        <span className={cls.StreamingTargetLabel}>
          {localize("#StreamingClient_StreamFrom", "Stream from: %s", host.name)}
          {!host.online && <span style={{ opacity: 0.6 }}> · asleep</span>}
        </span>
        <span style={{ marginLeft: "auto", paddingLeft: "1em", display: "inline-flex", opacity: 0.85 }}>
          <PunktfunkMark size={18} />
        </span>
      </MenuItem>,
    );
  }
  showContextMenu(<Menu label={localize("#GameAction_PlayFrom", "Play from")}>{items}</Menu>, anchor);
}

// ---- The Play button ------------------------------------------------------------------------

const DROPDOWN_KEY = "punktfunk-play-from";

/** Steam's ▾ where Steam left it out: the main button's own component in Steam's selector
 *  classes, in Steam's place after the button and before the explainer. False when the output
 *  has another shape. */
function addDropdown(out: any, main: any, onClick: (e: any) => void): boolean {
  const cls = classes();
  const group = collectElements(
    out,
    (x) => Array.isArray(x?.props?.children) && x.props.children.includes(main),
  )[0];
  if (!group || !cls.StreamingSelector) {
    return false;
  }
  const kids = group.props.children as any[];
  if (kids.some((k) => k?.key === DROPDOWN_KEY)) {
    return true;
  }
  kids.splice(
    Math.max(kids.indexOf(main) + 1, kids.length - 1),
    0,
    createElement(
      main.type,
      {
        key: DROPDOWN_KEY,
        noFocusRing: true,
        className: `${cls.StreamingSelector} ${cls.ButtonChild ?? ""}`,
        onClick,
        "aria-label": localize("#GameAction_PlayFrom", "Play from"),
      },
      // Steam's caret is not exported: the same triangle on its 36-unit grid, sized by its CSS.
      <svg viewBox="0 0 36 36" aria-hidden="true">
        <path d="M18 26.5 3.2 11.8h29.6z" fill="currentColor" />
      </svg>,
    ),
  );
  group.props.className = `${group.props.className ?? ""} ${cls.ShowingStreaming ?? ""}`;
  return true;
}

/** Steam's Play button class renders [main button, ▾, explainer]. Re-point the ▾ at our menu,
 *  draw it where Steam left it out, and while a Punktfunk host is chosen, re-dress the main
 *  button as our Stream. */
const playButtonPatcher = createRenderPatcher((out, self) => {
  const overview = (self as any)?.props?.overview as Overview | undefined;
  if (!out || !overview || typeof overview.appid !== "number") {
    return out;
  }
  instances.add(self);
  ensureWatching();
  const game = gameOf(overview);

  // Only where we have a host to add. The rebuilt menu leaves out Steam's own "Play on another
  // device" explainer, so a title no host carries keeps Steam's menu exactly as Steam drew it.
  const hosts = hostsForApp(game.appId, getHostStore().views);
  if (hosts.length === 0) {
    return out;
  }
  const open = (e: any) => openPlayFromMenu(overview, e?.currentTarget ?? undefined);
  const dropdown = collectElements(
    out,
    (x) => !!x?.props && "overview" in x.props && typeof x.props.onClick === "function" && !Array.isArray(x.props.children),
  )[0];
  if (dropdown) {
    dropdown.props.onClick = open;
  }
  const main = collectElements(
    out,
    (x) => !!x?.props && Array.isArray(x.props.children) && typeof x.props.onClick === "function" && "onFocus" in x.props,
  )[0];
  if (!main) {
    diag(`play-from ${game.appId}: main button not found in ${describe(out)}`);
    return out;
  }
  // The ▾ is the only way back to "This device", so the button stays Steam's without one.
  if (!dropdown && !addDropdown(out, main, open)) {
    diag(`play-from ${game.appId}: no place for the ▾ in ${describe(out)}`);
    return out;
  }

  const host = selectedHost(game.appId, hosts);
  if (!host) {
    return out;
  }
  const streaming = isGameStreaming(game.appId);
  main.props.className = `${main.props.className ?? ""} punktfunk-play`;
  main.props.onClick = () => (streaming ? stopGameStream(game.appId) : streamFrom(host, game));
  const kids = main.props.children as any[];
  const iconIdx = kids.findIndex((k) => k && typeof k === "object" && k.type && typeof k.type !== "string");
  const labelIdx = kids.findIndex((k) => k && k.type === "div");
  if (iconIdx >= 0) {
    kids[iconIdx] = <PunktfunkMark key="pf-mark" size={26} />;
  }
  if (labelIdx >= 0) {
    kids[labelIdx] = cloneElement(kids[labelIdx], {}, streaming ? "Stop" : "Stream");
  }
  return out;
}, "play-from");

/** The forwardRef around the Play button class: its render output is the class element. */
const playWrapperPatcher = createRenderPatcher((out) => {
  const isPlayButton = (x: any) => typeof x?.type === "function" && !!x.type.prototype?.ShowStreamingMenu;
  const el = isPlayButton(out) ? out : collectElements(out, isPlayButton)[0];
  if (el) {
    playButtonPatcher.patch(el);
  } else {
    diag(`play-from: Play button class not found in ${describe(out)}`);
  }
  return out;
}, "play-from");

/** Called with the play bar row's render output: hooks the Play group's component chain. */
export function patchPlayGroup(rowOut: any): void {
  const wrapper = collectElements(rowOut, (x) => !!x?.props && "bShowStreamingSelector" in x.props)[0];
  if (wrapper) {
    playWrapperPatcher.patch(wrapper);
  }
}

export function resetPlayFrom(): void {
  playButtonPatcher.reset();
  playWrapperPatcher.reset();
  instances.clear();
}
