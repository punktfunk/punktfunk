// One host in the QAM panel: one compact row. A ready host streams on a press, and its ▾ (or the
// controller's menu button) lists its profiles and pinned presets. A host that needs access opens
// the trust sheet. The host's bound preset is never listed: the plain press already applies it.
import {
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
import { type HostProfile, profiles as fetchProfiles } from "../backend";
import { type HostView, trustState } from "../hosts/model";
import { t } from "../i18n";
import { OsMark } from "../os-icon";
import { startStream } from "../stream";
import { TrustSheet } from "../trust/trust";
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

// The last answer per host ref, so a row has its ▾ the moment the panel opens; each open asks
// again. Module-level: the panel remounts on every open.
const playAsCache = new Map<string, PlayAs>();

function usePlayAs(host: HostView): PlayAs {
  const [answer, setAnswer] = useState(() => playAsCache.get(host.ref) ?? NOBODY);
  useEffect(() => {
    setAnswer(playAsCache.get(host.ref) ?? NOBODY);
    if (!host.online) {
      return;
    }
    let live = true;
    void fetchProfiles(host.ref)
      .then((r) => {
        const next = r.ok ? { profiles: r.profiles ?? [], picked: r.picked ?? null } : NOBODY;
        if (r.ok) playAsCache.set(host.ref, next);
        else playAsCache.delete(host.ref);
        if (live) setAnswer(next);
      })
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [host.ref, host.online]);
  return answer;
}

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

/** A ready host: press streams, ▾ and the menu button pick a profile or preset for this stream. */
export const HostRow: FC<{ host: HostView; showAddress: boolean }> = ({ host, showAddress }) => {
  const playAs = usePlayAs(host);
  const menu = rowMenu(host.pinnedPresets.length, playAs.profiles.length);
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
      </Menu>,
      caret.current ?? undefined,
    );
  return (
    <PanelSectionRow>
      <Focusable style={{ display: "flex", gap: "0.4em" }} flow-children="row">
        <DialogButton
          style={{ flex: 1, minWidth: 0 }}
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
            style={{ flex: "0 0 auto", width: "auto", minWidth: 0, padding: "0 0.8em" }}
            onClick={openMenu}
          >
            <FaCaretDown />
          </DialogButton>
        )}
      </Focusable>
    </PanelSectionRow>
  );
};

/** A host with no pinned fingerprint: press opens the trust sheet. */
export const AccessRow: FC<{ host: HostView; showAddress: boolean; refresh: () => void }> = ({
  host,
  showAddress,
  refresh,
}) => (
  <PanelSectionRow>
    <DialogButton
      style={{ width: "100%", minWidth: 0 }}
      onOKActionDescription={t("decky_host_connect")}
      onClick={() =>
        showModal(
          <TrustSheet host={host} onStream={() => void startStream(host)} onChanged={refresh} />,
        )
      }
    >
      <HostLabel host={host} icon={<FaLock />} showAddress={showAddress} />
    </DialogButton>
  </PanelSectionRow>
);
