// The Quick Access Menu panel: pending updates, the host rows, the console-home button, the
// game-page toggle, the host's own menus while a stream is up, and the recovery buttons. It
// mounts fresh on every QAM open.
import {
  ButtonItem,
  ConfirmModal,
  Field,
  PanelSection,
  PanelSectionRow,
  Spinner,
  showModal,
  ToggleField,
} from "@decky/ui";
import { FC, useEffect, useState } from "react";
import { FaDownload, FaGamepad, FaPlus, FaStopCircle, FaSyncAlt, FaTv } from "react-icons/fa";
import { streamRunning } from "../backend";
import { gamePageStreamEnabled, setGamePageStreamEnabled } from "../game-page/library-page";
import { useHosts } from "../hosts/store";
import { t } from "../i18n";
import { lastGameStream, launchGamepadUi } from "../steam/launch";
import { applyUpdate, checkForUpdatesNow, useUpdate } from "../update/actions";
import { updateBanner } from "../update/model";
import {
  endStreamedGame,
  forceStop,
  pressHost,
  recreatePunktfunkShortcut,
  removeGamePageShortcuts,
} from "./actions";
import { HostRow } from "./HostRow";

export const QamPanel: FC = () => {
  const { views, scanning, problem, refresh } = useHosts();
  const { info: update, checking, check } = useUpdate();
  const banner = updateBanner(update);
  // The host-buttons section shows only while the streaming client is up (checked per
  // panel open — the QAM panel mounts fresh each time).
  const [streaming, setStreaming] = useState(false);
  useEffect(() => {
    let live = true;
    void streamRunning()
      .then((r) => live && setStreaming(r.running))
      .catch(() => {});
    return () => {
      live = false;
    };
  }, []);
  const [gamePageStream, setGamePageStream] = useState(gamePageStreamEnabled);

  return (
    <>
      {banner !== "none" &&
        (banner === "manual-client" ? (
          <PanelSection title={t("decky_section_client_update")}>
            <PanelSectionRow>
              <Field
                focusable
                label={update!.client_latest || t("decky_newer_version")}
                description={update!.client_opt_in || update!.client_command}
              />
            </PanelSectionRow>
          </PanelSection>
        ) : (
          <PanelSection title={t("decky_section_update")}>
            <PanelSectionRow>
              <ButtonItem
                layout="below"
                onClick={() => applyUpdate(update!, check)}
                label={
                  banner === "plugin-and-client"
                    ? t("decky_update_plugin_client", {
                        current: update!.current,
                        latest: update!.latest,
                      })
                    : banner === "plugin"
                      ? t("decky_update_plugin", {
                          current: update!.current,
                          latest: update!.latest,
                        })
                      : t("decky_update_client_only")
                }
                description={t("decky_update_hint")}
              >
                <FaDownload style={{ marginRight: "0.5em" }} />
                {t("decky_update_button")}
              </ButtonItem>
            </PanelSectionRow>
          </PanelSection>
        ))}

      <PanelSection title={t("decky_section_hosts")}>
        <PanelSectionRow>
          <ButtonItem layout="below" onClick={() => void refresh()} disabled={scanning}>
            {scanning ? (
              <Spinner style={{ height: "1em", marginRight: "0.5em" }} />
            ) : (
              <FaSyncAlt style={{ marginRight: "0.5em" }} />
            )}
            {scanning ? t("decky_scanning") : t("decky_refresh")}
          </ButtonItem>
        </PanelSectionRow>
        {/* A client that is missing or too old explains itself rather than rendering an empty
            list — "no hosts on your LAN" would blame the network for the plugin's problem, and
            for the outdated case the button that fixes it is in this same panel. */}
        {problem && (
          <PanelSectionRow>
            <Field
              focusable={false}
              label={
                problem === "client-unavailable"
                  ? t("decky_problem_not_installed")
                  : problem === "list-failed"
                    ? t("decky_problem_scan_failed")
                    : t("decky_problem_outdated")
              }
              description={
                problem === "client-unavailable"
                  ? t("decky_problem_not_installed_body")
                  : problem === "list-failed"
                    ? t("decky_problem_scan_failed_body")
                    : t("decky_problem_outdated_body")
              }
            />
          </PanelSectionRow>
        )}
        {views.length === 0 && scanning && (
          <PanelSectionRow>
            <Field focusable={false} description={t("decky_scanning_network")} />
          </PanelSectionRow>
        )}
        {views.length === 0 && !scanning && !problem && (
          <PanelSectionRow>
            <Field
              focusable={false}
              label={t("hosts_empty_title")}
              description={t("decky_empty_body")}
            />
          </PanelSectionRow>
        )}
        {views.map((v) => (
          <HostRow key={v.ref} host={v} refresh={refresh} />
        ))}
      </PanelSection>

      <PanelSection title="Punktfunk">
        <PanelSectionRow>
          <ButtonItem
            layout="below"
            description={t("decky_open_app_hint")}
            onClick={() => void launchGamepadUi()}
          >
            <FaTv style={{ marginRight: "0.5em" }} />
            {t("decky_open_app")}
          </ButtonItem>
        </PanelSectionRow>
        <PanelSectionRow>
          <ToggleField
            label={t("decky_play_menu")}
            description={t("decky_play_menu_hint")}
            checked={gamePageStream}
            onChange={(on) => {
              setGamePageStreamEnabled(on);
              setGamePageStream(on);
            }}
          />
        </PanelSectionRow>
      </PanelSection>

      {streaming && (
        <PanelSection title={t("decky_section_host_menus")}>
          <PanelSectionRow>
            <ButtonItem
              layout="below"
              description={t("decky_host_steam_menu_hint")}
              onClick={() => void pressHost("guide")}
            >
              <FaGamepad style={{ marginRight: "0.5em" }} />
              {t("decky_host_steam_menu")}
            </ButtonItem>
          </PanelSectionRow>
          <PanelSectionRow>
            <ButtonItem
              layout="below"
              description={t("decky_host_qam_hint")}
              onClick={() => void pressHost("qam")}
            >
              <FaGamepad style={{ marginRight: "0.5em" }} />
              {t("decky_host_qam")}
            </ButtonItem>
          </PanelSectionRow>
          {lastGameStream() && (
            <PanelSectionRow>
              <ButtonItem
                layout="below"
                description={t("decky_end_game_hint", { title: lastGameStream()?.title ?? "" })}
                onClick={() =>
                  showModal(
                    <ConfirmModal
                      strTitle={t("decky_end_game_confirm_title", {
                        title: lastGameStream()?.title ?? "",
                      })}
                      strDescription={t("lib_end_game_confirm_body")}
                      strOKButtonText={t("lib_end_game")}
                      onOK={() => void endStreamedGame()}
                    />,
                  )
                }
              >
                <FaStopCircle style={{ marginRight: "0.5em" }} />
                {t("lib_end_game")}
              </ButtonItem>
            </PanelSectionRow>
          )}
        </PanelSection>
      )}

      <PanelSection title={t("decky_section_about")}>
        <PanelSectionRow>
          <Field
            focusable={false}
            label={t("about_version")}
            description={
              update
                ? update.channel
                  ? `v${update.current} · ${update.channel}`
                  : t("decky_version_dev", { version: update.current })
                : "…"
            }
          />
        </PanelSectionRow>
        <PanelSectionRow>
          <ButtonItem
            layout="below"
            disabled={checking}
            onClick={() => void checkForUpdatesNow(check)}
          >
            {checking ? t("decky_checking") : t("decky_check_updates")}
          </ButtonItem>
        </PanelSectionRow>
        <PanelSectionRow>
          <ButtonItem
            layout="below"
            description={t("decky_recreate_hint")}
            onClick={() => void recreatePunktfunkShortcut()}
          >
            <FaPlus style={{ marginRight: "0.5em" }} />
            {t("decky_recreate")}
          </ButtonItem>
        </PanelSectionRow>
        <PanelSectionRow>
          <ButtonItem
            layout="below"
            description={t("decky_remove_game_shortcuts_hint")}
            onClick={() => removeGamePageShortcuts()}
          >
            {t("decky_remove_game_shortcuts")}
          </ButtonItem>
        </PanelSectionRow>
        <PanelSectionRow>
          <ButtonItem
            layout="below"
            description={t("decky_force_stop_hint")}
            onClick={() => void forceStop()}
          >
            <FaStopCircle style={{ marginRight: "0.5em" }} />
            {t("decky_force_stop")}
          </ButtonItem>
        </PanelSectionRow>
      </PanelSection>
    </>
  );
};
