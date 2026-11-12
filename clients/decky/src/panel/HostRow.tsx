// One host in the QAM panel: the row that streams it or opens the trust sheet, its pinned cards,
// and one chip per profile on a box with several.
import { ButtonItem, PanelSectionRow, showModal } from "@decky/ui";
import { FC, useEffect, useState } from "react";
import { FaLock, FaPlay } from "react-icons/fa";
import { type HostProfile, profiles as fetchProfiles } from "../backend";
import { type HostView, needsPair, trustState } from "../hosts/model";
import { t } from "../i18n";
import { OsMark } from "../os-icon";
import { startStream } from "../stream";
import { TrustSheet } from "../trust/trust";

/** The line under a host's name: where it is, whether it's up, and how far trust has got. */
function hostDescription(v: HostView): string {
  const trust = {
    paired: t("hostcard_paired"),
    trusted: t("decky_host_trusted"),
    "needs-access": t("decky_host_needs_access"),
  }[trustState(v)];
  const online = v.online ? t("hostcard_status_online") : t("hostcard_status_offline");
  return `${v.addr}:${v.port} · ${online} · ${trust}`;
}

/** The profiles of a paired, online host. Empty for a host with one profile or none: no chips. */
function useHostProfiles(host: HostView, gated: boolean): HostProfile[] {
  const [list, setList] = useState<HostProfile[]>([]);
  useEffect(() => {
    if (gated || !host.online) {
      setList([]);
      return;
    }
    let live = true;
    void fetchProfiles(host.ref)
      .then((r) => live && setList(r.ok && (r.profiles?.length ?? 0) > 1 ? r.profiles! : []))
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [host.ref, host.online, gated]);
  return list;
}

export const HostRow: FC<{ host: HostView; refresh: () => void }> = ({ host, refresh }) => {
  const gated = needsPair(host);
  const people = useHostProfiles(host, gated);
  const stream = (opts: { requestAccess?: boolean } = {}) => void startStream(host, opts);
  return (
    <>
      <PanelSectionRow>
        <ButtonItem
          layout="below"
          onClick={() =>
            gated
              ? showModal(
                  <TrustSheet host={host} onStream={stream} onChanged={refresh} />,
                )
              : stream()
          }
          label={
            <span style={{ display: "inline-flex", alignItems: "center", gap: "0.4em" }}>
              {gated ? <FaLock /> : <OsMark os={host.os} />}
              {host.name}
            </span>
          }
          description={hostDescription(host)}
        >
          {gated ? t("decky_host_connect") : t("decky_stream")}
        </ButtonItem>
      </PanelSectionRow>
      {/* Pinned cards, nested under their host rather than in a section of their own: a card
          IS a (host, preset) pair, and a row that floats free of its host is the "a pinned
          tile reads as a duplicate host" problem the desktop shells still have. The host's
          own BOUND preset is deliberately not a card — it applies silently on the plain row
          above, and showing it twice would suggest they do different things. */}
      {!gated &&
        host.pinnedPresets.map((p) => (
          <PanelSectionRow key={`${host.ref}:${p.id}`}>
            <ButtonItem
              layout="below"
              onClick={() => void startStream(host, { presetId: p.id }, { preset: p.name })}
              label={`▸ ${p.name}`}
            >
              <FaPlay style={{ marginRight: "0.5em" }} />
              {t("decky_stream")}
            </ButtonItem>
          </PanelSectionRow>
        ))}
      {/* One chip per profile on a box with several: streams as that player, this time only. */}
      {people.map((p) => (
        <PanelSectionRow key={`${host.ref}:as:${p.id}`}>
          <ButtonItem
            layout="below"
            onClick={() => void startStream(host, { profileId: p.id }, { profile: p.display_name })}
            label={
              <span style={{ display: "inline-flex", alignItems: "center", gap: "0.4em" }}>
                {p.picture ? (
                  <img
                    src={p.picture}
                    alt=""
                    style={{ width: "1.2em", height: "1.2em", borderRadius: "50%", objectFit: "cover" }}
                  />
                ) : (
                  "●"
                )}
                {p.display_name}
              </span>
            }
            description={p.note ?? undefined}
          >
            <FaPlay style={{ marginRight: "0.5em" }} />
            {t("decky_stream")}
          </ButtonItem>
        </PanelSectionRow>
      ))}
    </>
  );
};
