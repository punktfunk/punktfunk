// While a stream is up: the host's own menus, End game after a game-page stream, and Stop. It
// follows Steam's running feed, so it comes and goes with the panel open. A connect parked for
// approval runs our shortcut before the client's control socket exists; Stop must reach it too.
import { ButtonItem, ConfirmModal, PanelSection, PanelSectionRow, showModal } from "@decky/ui";
import { FC, useEffect, useState } from "react";
import { FaGamepad, FaStopCircle } from "react-icons/fa";
import { streamRunning } from "../backend";
import { t } from "../i18n";
import { lastGameStream } from "../steam/launch";
import { isAnyStreamRunning, subscribeRunning } from "../steam/running";
import { endStreamedGame, forceStop, pressHost } from "./actions";
import { streamIsUp } from "./model";

/** Asked on mount and on every change Steam reports; the latest answer wins. */
function useStreamUp(): boolean {
  const [up, setUp] = useState(isAnyStreamRunning);
  useEffect(() => {
    let asked = 0;
    let live = true;
    const ask = () => {
      const n = ++asked;
      const settle = (socket: boolean) => live && n === asked && setUp(streamIsUp(socket, isAnyStreamRunning()));
      void streamRunning()
        .then((r) => settle(r.running))
        .catch(() => settle(false));
    };
    ask();
    const unsubscribe = subscribeRunning(ask);
    return () => {
      live = false;
      unsubscribe();
    };
  }, []);
  return up;
}

export const SessionSection: FC = () => {
  const up = useStreamUp();
  if (!up) {
    return null;
  }
  const game = lastGameStream();
  return (
    <PanelSection title={t("hostcard_status_streaming")}>
      <PanelSectionRow>
        <ButtonItem layout="below" description={t("decky_host_steam_menu_hint")} onClick={() => void pressHost("guide")}>
          <FaGamepad style={{ marginRight: "0.5em" }} />
          {t("decky_host_steam_menu")}
        </ButtonItem>
      </PanelSectionRow>
      <PanelSectionRow>
        <ButtonItem layout="below" description={t("decky_host_qam_hint")} onClick={() => void pressHost("qam")}>
          <FaGamepad style={{ marginRight: "0.5em" }} />
          {t("decky_host_qam")}
        </ButtonItem>
      </PanelSectionRow>
      {game && (
        <PanelSectionRow>
          <ButtonItem
            layout="below"
            description={t("decky_end_game_hint", { title: game.title })}
            onClick={() =>
              showModal(
                <ConfirmModal
                  strTitle={t("decky_end_game_confirm_title", { title: game.title })}
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
      <PanelSectionRow>
        <ButtonItem layout="below" onClick={() => void forceStop()}>
          <FaStopCircle style={{ marginRight: "0.5em" }} />
          {t("decky_play_stop")}
        </ButtonItem>
      </PanelSectionRow>
    </PanelSection>
  );
};
