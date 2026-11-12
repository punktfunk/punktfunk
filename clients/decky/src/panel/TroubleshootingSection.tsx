// The repairs: end a wedged stream, put the library entry back, tidy the per-game entries. Each
// ends in a toast, so a tap has feedback even when it found nothing to do.
import { ButtonItem, PanelSection, PanelSectionRow } from "@decky/ui";
import { FC } from "react";
import { FaPlus, FaStopCircle } from "react-icons/fa";
import { t } from "../i18n";
import { forceStop, recreatePunktfunkShortcut, removeGamePageShortcuts } from "./actions";

export const TroubleshootingSection: FC = () => (
  <PanelSection title={t("decky_section_troubleshooting")}>
    <PanelSectionRow>
      <ButtonItem layout="below" description={t("decky_force_stop_hint")} onClick={() => void forceStop()}>
        <FaStopCircle style={{ marginRight: "0.5em" }} />
        {t("decky_force_stop")}
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
  </PanelSection>
);
