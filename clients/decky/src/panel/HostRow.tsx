// One host in the QAM panel: one compact row. A ready host streams on a press, and its ▾ (or the
// controller's menu button) lists its profiles, pinned presets, the host's Screen section, and
// Wake or the host's own power actions. A host that needs access opens the trust sheet. The
// host's bound preset is never listed: the plain press already applies it.
import {
  ConfirmModal,
  DialogButton,
  Focusable,
  Menu,
  MenuItem,
  MenuSeparator,
  PanelSectionRow,
  showContextMenu,
  showModal,
} from "@decky/ui";
import { CSSProperties, FC, Fragment, ReactNode, useEffect, useRef, useState } from "react";
import { FaCaretDown, FaCheck, FaLock } from "react-icons/fa";
import {
  actions as fetchActions,
  type HostAction,
  type HostProfile,
  profiles as fetchProfiles,
  type ScreenNote,
  type ScreenRow,
  screens as fetchScreens,
  type ScreensResult,
} from "../backend";
import { type HostView, trustState } from "../hosts/model";
import { t } from "../i18n";
import { toast } from "../lib/toast";
import { OsMark } from "../os-icon";
import { startStream } from "../stream";
import { TrustSheet } from "../trust/trust";
import { actionLabel, pickHostScreen, runHostAction, screenLabel, wakeHost } from "./actions";
import { rowMenu, rowStatus, screenPress } from "./model";

const ELLIPSIS: CSSProperties = {
  minWidth: 0,
  overflow: "hidden",
  textOverflow: "ellipsis",
  whiteSpace: "nowrap",
};

/** The name on its own line, so status words never push it out; status below, small. */
const HostLabel: FC<{ host: HostView; icon: ReactNode; showAddress: boolean }> = ({
  host,
  icon,
  showAddress,
}) => {
  const status = rowStatus(host);
  const notes = [
    status === "offline-wakes" ? t("hostcard_status_offline_wakes") : null,
    status === "offline" ? t("hostcard_status_offline") : null,
    trustState(host) === "trusted" ? t("decky_host_trusted") : null,
    showAddress ? `${host.addr}:${host.port}` : null,
  ].filter(Boolean);
  return (
    <div style={{ display: "flex", flexDirection: "column", width: "100%", minWidth: 0, textAlign: "left" }}>
      <div style={{ display: "flex", alignItems: "center", gap: "0.4em", minWidth: 0 }}>
        {icon}
        <span style={ELLIPSIS}>{host.name}</span>
      </div>
      {notes.length > 0 && (
        <div style={{ ...ELLIPSIS, fontSize: "0.8em", opacity: 0.7 }}>{notes.join(" · ")}</div>
      )}
    </div>
  );
};

interface PlayAs {
  profiles: HostProfile[];
  picked: string | null;
}
const NOBODY: PlayAs = { profiles: [], picked: null };
const NO_ACTIONS: HostAction[] = [];

/** The host's Screen section: its rows, and why none can be picked. */
interface Screens {
  rows: ScreenRow[];
  note: ScreenNote | null;
  preset: string | null;
}
const NO_SCREENS: Screens = { rows: [], note: null, preset: null };

async function askPlayAs(ref: string): Promise<PlayAs | null> {
  const r = await fetchProfiles(ref);
  return r.ok ? { profiles: r.profiles ?? [], picked: r.picked ?? null } : null;
}

async function askActions(ref: string): Promise<HostAction[] | null> {
  const r = await fetchActions(ref);
  return r.ok ? (r.actions ?? []) : null;
}

const toScreens = (r: ScreensResult): Screens => ({
  rows: r.rows ?? [],
  note: r.note ?? null,
  preset: r.preset ?? null,
});

async function askScreens(ref: string): Promise<Screens | null> {
  const r = await fetchScreens(ref);
  return r.ok ? toScreens(r) : null;
}

// The last answer per host ref, so a row has its ▾ the moment the panel opens. Module-level: the
// panel remounts on every open.
const playAsCache = new Map<string, PlayAs>();
const actionsCache = new Map<string, HostAction[]>();
const screensCache = new Map<string, Screens>();

/** The cached answer for `ref` at once, then `ask`'s on each mount while `asking`. A null
 *  answer drops the cached one. The setter stores an answer got another way, as after a pick. */
function useHostAnswer<T>(
  cache: Map<string, T>,
  ref: string,
  asking: boolean,
  none: T,
  ask: (ref: string) => Promise<T | null>,
): [T, (next: T) => void] {
  const [answer, setAnswer] = useState(() => cache.get(ref) ?? none);
  useEffect(() => {
    setAnswer(cache.get(ref) ?? none);
    if (!asking) {
      return;
    }
    let live = true;
    void ask(ref)
      .then((next) => {
        if (next) cache.set(ref, next);
        else cache.delete(ref);
        if (live) setAnswer(next ?? none);
      })
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [ref, asking]);
  const put = (next: T) => {
    cache.set(ref, next);
    setAnswer(next);
  };
  return [answer, put];
}

// One block per group: the list owns the rounded corners and a hairline between rows, so a row
// and its ▾ are square and read as one control.
const HAIRLINE = "1px";
const SQUARE: CSSProperties = { borderRadius: 0, minWidth: 0 };

/** The rows of one group, drawn as a single list. */
export const HostList: FC<{ children: ReactNode }> = ({ children }) => {
  return (
    <PanelSectionRow>
      <div style={{ display: "flex", flexDirection: "column", gap: HAIRLINE, borderRadius: "3px", overflow: "hidden" }}>
        {children}
      </div>
    </PanelSectionRow>
  );
};

/** A menu row's check column: a check on the one in use, else blank, so the names line up. */
const CheckSlot: FC<{ on: boolean }> = ({ on }) => {
  return <span style={{ width: "1em", flexShrink: 0 }}>{on && <FaCheck style={{ color: "#1a9fff" }} />}</span>;
};

/** A profile row: a check on the one this Deck plays as, its picture, its name, its note. */
const ProfileItem: FC<{ profile: HostProfile; picked: boolean }> = ({ profile: p, picked }) => {
  return (
    <span style={{ display: "flex", alignItems: "center", gap: "0.5em", minWidth: 0 }}>
      <CheckSlot on={picked} />
      {p.picture ? (
        <img
          src={p.picture}
          alt=""
          style={{ width: "1.2em", height: "1.2em", borderRadius: "50%", objectFit: "cover", flexShrink: 0 }}
        />
      ) : (
        "●"
      )}
      <span style={ELLIPSIS}>{p.display_name}</span>
      {p.note && <span style={{ ...ELLIPSIS, opacity: 0.6, fontSize: "0.85em" }}>{p.note}</span>}
    </span>
  );
};

/** A host action row: a danger one asks first, an unavailable one says why when pressed. */
function actionItem(host: HostView, a: HostAction): ReactNode {
  const label = actionLabel(a);
  const run = () => void runHostAction(host, a);
  const confirm = () => {
    showModal(
      <ConfirmModal
        strTitle={t("host_power_confirm_title", { action: label })}
        strDescription={t("hostcard_action_confirm_body", { host: host.name })}
        strOKButtonText={label}
        onOK={run}
      />,
    );
  };
  return (
    <MenuItem key={`do:${a.id}`} onSelected={a.danger && a.available ? confirm : run}>
      {a.available ? label : t("hostcard_action_unavailable", { action: label })}
    </MenuItem>
  );
}

/** A line of the ▾ menu that is no row: a group's title, or why its rows can't be picked. The
 *  controller passes over it; 14px is the side padding of Steam's menu rows. */
const MenuCaption: FC<{ children: ReactNode }> = ({ children }) => {
  return (
    <div style={{ display: "flex", alignItems: "center", gap: "0.4em", padding: "8px 14px 4px", fontSize: "0.8em", opacity: 0.7 }}>
      {children}
    </div>
  );
};

/** Why a monitor can't be streamed now. */
function screenReason(row: ScreenRow): string | null {
  if (row.in_use_by) return t("screen_in_use", { device: row.in_use_by });
  return row.off ? t("screen_off") : null;
}

/** What a role does, for the line under it. */
function roleHint(row: ScreenRow): string | null {
  switch (row.kind === "role" ? row.id : "") {
    case "only":
      return t("screen_role_only_hint");
    case "main":
      return t("screen_role_main_hint");
    case "second":
      return t("screen_role_second_hint");
    default:
      return null;
  }
}

/** Why no Screen row can be picked: the host's settings file, this Deck's access, or the preset
 *  the host is bound to. */
function screenNote(s: Screens): string | null {
  switch (s.note) {
    case "locked":
      return t("screen_locked_hint");
    case "host_decides":
      return t("screen_host_decides");
    case "preset":
      return t("screen_preset_decides", { preset: s.preset ?? "" });
    default:
      return null;
  }
}

/**
 * A Screen row: a check on what a connect gets now, the row's name and value, and why it is
 * greyed, else what the role does, below. The current row does nothing; a greyed one says why.
 * "Show a monitor" heads the first monitor.
 */
function screenItem(host: HostView, row: ScreenRow, note: string | null, put: (s: Screens) => void): ReactNode {
  const reason = screenReason(row);
  const why = reason ?? roleHint(row);
  const pick = () => void pickHostScreen(host, row).then((r) => r && put(toScreens(r)));
  const explain = () => {
    const said = reason ?? note;
    if (said) toast(said);
  };
  const act = { pick, greyed: explain, current: undefined }[screenPress(row)];
  return (
    <Fragment key={`screen:${row.id}`}>
      {row.header && <MenuCaption>{t("screen_show_monitor")}</MenuCaption>}
      <MenuItem onSelected={act}>
        <span style={{ display: "flex", alignItems: "center", gap: "0.5em", minWidth: 0 }}>
          <CheckSlot on={row.checked} />
          <span style={{ display: "flex", flexDirection: "column", minWidth: 0 }}>
            <span style={{ display: "flex", gap: "0.5em", minWidth: 0, opacity: row.pickable ? 1 : 0.5 }}>
              <span style={ELLIPSIS}>{screenLabel(row)}</span>
              {row.detail && <span style={{ ...ELLIPSIS, opacity: 0.6, fontSize: "0.85em" }}>{row.detail}</span>}
            </span>
            {why && <span style={{ fontSize: "0.8em", opacity: 0.7 }}>{why}</span>}
          </span>
        </span>
      </MenuItem>
    </Fragment>
  );
}

/**
 * A ready host: press streams. ▾ and the menu button pick a profile or preset for this stream,
 * the screen this Deck gets (online, paired), and hold Wake (offline, MAC known) or the actions
 * the host grants this Deck (online, paired).
 */
export const HostRow: FC<{ host: HostView; showAddress: boolean }> = ({ host, showAddress }) => {
  const asking = host.online && host.paired;
  const [playAs] = useHostAnswer(playAsCache, host.ref, host.online, NOBODY, askPlayAs);
  const [granted] = useHostAnswer(actionsCache, host.ref, asking, NO_ACTIONS, askActions);
  const [screen, putScreen] = useHostAnswer(screensCache, host.ref, asking, NO_SCREENS, askScreens);
  const power = host.online ? granted : NO_ACTIONS;
  const shown = host.online ? screen : NO_SCREENS;
  const note = screenNote(shown);
  const canWake = !host.online && host.wakeable;
  const menu = rowMenu(
    host.pinnedPresets.length,
    playAs.profiles.length,
    power.length + (canWake ? 1 : 0),
    shown.rows.length + (note ? 1 : 0),
  );
  const caret = useRef<HTMLDivElement>(null);
  const openMenu = () =>
    menu &&
    showContextMenu(
      <Menu label={host.name}>
        {menu.profiles &&
          playAs.profiles.map((p) => (
            <MenuItem
              key={`as:${p.id}`}
              onSelected={() => void startStream(host, { profileId: p.id }, { profile: p.display_name })}
            >
              <ProfileItem profile={p} picked={p.id === playAs.picked} />
            </MenuItem>
          ))}
        {menu.profiles && menu.presets && <MenuSeparator />}
        {menu.presets &&
          host.pinnedPresets.map((p) => (
            <MenuItem
              key={`preset:${p.id}`}
              onSelected={() => void startStream(host, { presetId: p.id }, { preset: p.name })}
            >
              ▸ {p.name}
            </MenuItem>
          ))}
        {menu.screen && (menu.profiles || menu.presets) && <MenuSeparator />}
        {menu.screen && <MenuCaption>{t("hostcard_section_screen")}</MenuCaption>}
        {shown.rows.map((r) => screenItem(host, r, note, putScreen))}
        {note && (
          <MenuCaption>
            <FaLock />
            {note}
          </MenuCaption>
        )}
        {menu.power && (menu.profiles || menu.presets || menu.screen) && <MenuSeparator />}
        {canWake && <MenuItem onSelected={() => void wakeHost(host)}>{t("hostcard_wake")}</MenuItem>}
        {power.map((a) => actionItem(host, a))}
      </Menu>,
      caret.current ?? undefined,
    );
  return (
    <Focusable style={{ display: "flex", gap: HAIRLINE }} flow-children="row">
        <DialogButton
          style={{ ...SQUARE, flex: 1 }}
          onClick={() => void startStream(host)}
          onOKActionDescription={t("decky_stream")}
          onMenuButton={menu ? openMenu : undefined}
          onMenuActionDescription={menu ? t("hostcard_more_a11y") : undefined}
        >
          <HostLabel host={host} icon={<OsMark os={host.os} />} showAddress={showAddress} />
        </DialogButton>
        {menu && (
          <DialogButton
            ref={caret}
            style={{ ...SQUARE, flex: "0 0 auto", width: "auto", padding: "0 0.9em" }}
            onClick={openMenu}
          >
            <FaCaretDown />
          </DialogButton>
        )}
    </Focusable>
  );
};

/** A host with no pinned fingerprint: press opens the trust sheet. */
export const AccessRow: FC<{ host: HostView; showAddress: boolean; refresh: () => void }> = ({
  host,
  showAddress,
  refresh,
}) => (
  <DialogButton
      style={{ ...SQUARE, width: "100%" }}
      onOKActionDescription={t("decky_host_connect")}
      onClick={() =>
        showModal(
          <TrustSheet host={host} onStream={() => void startStream(host)} onChanged={refresh} />,
        )
      }
    >
      <HostLabel host={host} icon={<FaLock />} showAddress={showAddress} />
  </DialogButton>
);
