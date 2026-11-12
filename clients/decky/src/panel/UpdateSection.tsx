// The update row at the top while an update waits, and About at the bottom: the version and the
// explicit check. Both read the one check `useUpdate` runs per panel open.
import { ButtonItem, Field, PanelSection, PanelSectionRow } from "@decky/ui";
import { FC } from "react";
import { FaDownload } from "react-icons/fa";
import type { UpdateInfo } from "../backend";
import { t } from "../i18n";
import { applyUpdate, checkForUpdatesNow } from "../update/actions";
import { updateBanner } from "../update/model";

type Check = (force: boolean) => Promise<UpdateInfo | null>;

/** The button's label: what this one tap installs. */
function updateLabel(info: UpdateInfo): string {
  switch (updateBanner(info)) {
    case "plugin-and-client":
      return t("decky_update_plugin_client", { current: info.current, latest: info.latest });
    case "plugin":
      return t("decky_update_plugin", { current: info.current, latest: info.latest });
    default:
      return t("decky_update_client_only");
  }
}

export const UpdateSection: FC<{ info: UpdateInfo | null; check: Check }> = ({ info, check }) => {
  const banner = updateBanner(info);
  if (!info || banner === "none") {
    return null;
  }
  if (banner === "manual-client") {
    return (
      <PanelSection title={t("decky_section_client_update")}>
        <PanelSectionRow>
          <Field
            focusable
            label={info.client_latest || t("decky_newer_version")}
            description={info.client_opt_in || info.client_command}
          />
        </PanelSectionRow>
      </PanelSection>
    );
  }
  return (
    <PanelSection title={t("decky_section_update")}>
      <PanelSectionRow>
        <ButtonItem
          layout="below"
          onClick={() => applyUpdate(info, check)}
          label={updateLabel(info)}
          description={t("decky_update_hint")}
        >
          <FaDownload style={{ marginRight: "0.5em" }} />
          {t("decky_update_button")}
        </ButtonItem>
      </PanelSectionRow>
    </PanelSection>
  );
};

export const AboutSection: FC<{ info: UpdateInfo | null; checking: boolean; check: Check }> = ({
  info,
  checking,
  check,
}) => (
  <PanelSection title={t("decky_section_about")}>
    <PanelSectionRow>
      <Field
        focusable={false}
        label={t("about_version")}
        description={
          !info
            ? "…"
            : info.channel
              ? `v${info.current} · ${info.channel}`
              : t("decky_version_dev", { version: info.current })
        }
      />
    </PanelSectionRow>
    <PanelSectionRow>
      <ButtonItem layout="below" disabled={checking} onClick={() => void checkForUpdatesNow(check)}>
        {checking ? t("decky_checking") : t("decky_check_updates")}
      </ButtonItem>
    </PanelSectionRow>
  </PanelSection>
);
