// One host in the QAM panel: one compact row. A ready host streams on a press, and its ▾ (or the
// controller's menu button) lists its profiles, pinned presets, and Wake or the host's own power
// actions. A host that needs access opens the trust sheet. The host's bound preset is never
// listed: the plain press already applies it.
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
import { CSSProperties, FC, ReactNode, useEffect, useRef, useState } from "react";
import { FaCaretDown, FaCheck, FaLock } from "react-icons/fa";
import {
  actions as fetchActions,
  type HostAction,
  type HostProfile,
  profiles as fetchProfiles,
} from "../backend";
import { type HostView, trustState } from "../hosts/model";
import { t } from "../i18n";
import { OsMark } from "../os-icon";
import { startStream } from "../stream";
import { TrustSheet } from "../trust/trust";
import { actionLabel, runHostAction, wakeHost } from "./actions";
import { rowMenu, rowStatus } from "./model";

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

async function askPlayAs(ref: string): Promise<PlayAs | null> {
  const r = await fetchProfiles(ref);
  return r.ok ? { profiles: r.profiles ?? [], picked: r.picked ?? null } : null;
}

async function askActions(ref: string): Promise<HostAction[] | null> {
  const r = await fetchActions(ref);
  return r.ok ? (r.actions ?? []) : null;
}

// The last answer per host ref, so a row has its ▾ the moment the panel opens. Module-level: the
// panel remounts on every open.
const playAsCache = new Map<string, PlayAs>();
const actionsCache = new Map<string, HostAction[]>();

/** The cached answer for `ref` at once, then `ask`'s on each mount while `asking`. A null
 *  answer drops the cached one. */
function useHostAnswer<T>(
  cache: Map<string, T>,
  ref: string,
  asking: boolean,
  none: T,
  ask: (ref: string) => Promise<T | null>,
): T {
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
  return answer;
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

/** A profile row: a check on the one this Deck plays as, its picture, its name, its note. */
const ProfileItem: FC<{ profile: HostProfile; picked: boolean }> = ({ profile: p, picked }) => {
  return (
    <span style={{ display: "flex", alignItems: "center", gap: "0.5em", minWidth: 0 }}>
      <span style={{ width: "1em", flexShrink: 0 }}>{picked && <FaCheck style={{ color: "#1a9fff" }} />}</span>
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
  const run = () =>
    a.danger && a.available
      ? showModal(
          <ConfirmModal
            strTitle={t("host_power_confirm_title", { action: label })}
            strDescription={t("hostcard_action_confirm_body", { host: host.name })}
            strOKButtonText={label}
            onOK={() => void runHostAction(host, a)}
          />,
        )
      : void runHostAction(host, a);
  return (
    <MenuItem key={`do:${a.id}`} onSelected={run}>
      {a.available ? label : t("hostcard_action_unavailable", { action: label })}
    </MenuItem>
  );
}

/**
 * A ready host: press streams. ▾ and the menu button pick a profile or preset for this stream,
 * and hold Wake (offline, MAC known) or the actions the host grants this Deck (online, paired).
 */
export const HostRow: FC<{ host: HostView; showAddress: boolean }> = ({ host, showAddress }) => {
  const playAs = useHostAnswer(playAsCache, host.ref, host.online, NOBODY, askPlayAs);
  const granted = useHostAnswer(actionsCache, host.ref, host.online && host.paired, NO_ACTIONS, askActions);
  const power = host.online ? granted : NO_ACTIONS;
  const canWake = !host.online && host.wakeable;
  const menu = rowMenu(host.pinnedPresets.length, playAs.profiles.length, power.length + (canWake ? 1 : 0));
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
        {menu.power && (menu.profiles || menu.presets) && <MenuSeparator />}
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
