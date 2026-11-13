// The Quick Access Menu panel, state first: a pending update, the running stream, the hosts, the
// app, the repairs, About. Focus follows that order, so with no stream and no update the first
// press lands on a host. It mounts fresh on every QAM open.
import { ButtonItem, PanelSection, PanelSectionRow, ToggleField } from "@decky/ui";
import { FC, useState } from "react";
import { FaTv } from "react-icons/fa";
import { gamePageStreamEnabled, setGamePageStreamEnabled } from "../game-page/library-page";
import { t } from "../i18n";
import { launchGamepadUi } from "../steam/launch";
import { useUpdate } from "../update/actions";
import { HostsSection } from "./HostsSection";
import { SessionSection } from "./SessionSection";
import { TroubleshootingSection } from "./TroubleshootingSection";
import { AboutSection, UpdateSection } from "./UpdateSection";

const PunktfunkSection: FC = () => {
  const [gamePageStream, setGamePageStream] = useState(gamePageStreamEnabled);
  return (
    <PanelSection title="Punktfunk">
      <PanelSectionRow>
        <ButtonItem layout="below" description={t("decky_open_app_hint")} onClick={() => void launchGamepadUi()}>
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
  );
};

export const QamPanel: FC = () => {
  const { info, checking, check } = useUpdate();
  return (
    <>
      <UpdateSection info={info} check={check} />
      <SessionSection />
      <HostsSection />
      <PunktfunkSection />
      <TroubleshootingSection />
      <AboutSection info={info} checking={checking} check={check} />
    </>
  );
};
