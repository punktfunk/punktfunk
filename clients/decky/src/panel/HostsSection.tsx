// The host area: hosts this Deck can stream, then hosts that need access, then Refresh as the last
// row of whichever section comes last. A missing or outdated client explains itself in place of an
// empty list: "no hosts on your network" would blame the network for the plugin's problem.
import { ButtonItem, Field, PanelSection, PanelSectionRow, Spinner } from "@decky/ui";
import { FC } from "react";
import { FaSyncAlt } from "react-icons/fa";
import { splitByAccess } from "../hosts/model";
import { useHosts } from "../hosts/store";
import { t } from "../i18n";
import { AccessRow, HostRow } from "./HostRow";
import { sharedNames } from "./model";

const ProblemRow: FC<{ problem: string }> = ({ problem }) => (
  <PanelSectionRow>
    {problem === "client-unavailable" ? (
      <Field focusable={false} label={t("decky_problem_not_installed")} description={t("decky_problem_not_installed_body")} />
    ) : problem === "list-failed" ? (
      <Field focusable={false} label={t("decky_problem_scan_failed")} description={t("decky_problem_scan_failed_body")} />
    ) : (
      <Field focusable={false} label={t("decky_problem_outdated")} description={t("decky_problem_outdated_body")} />
    )}
  </PanelSectionRow>
);

export const HostsSection: FC = () => {
  const { views, scanning, problem, refresh } = useHosts();
  const { ready, access } = splitByAccess(views);
  const shared = sharedNames(views);
  const refreshRow = (
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
  );
  return (
    <>
      {/* With only hosts that need access, there is nothing to say under "Hosts". */}
      {(problem || ready.length > 0 || access.length === 0) && (
        <PanelSection title={t("decky_section_hosts")}>
          {problem && <ProblemRow problem={problem} />}
          {views.length === 0 && scanning && (
            <PanelSectionRow>
              <Field focusable={false} description={t("decky_scanning_network")} />
            </PanelSectionRow>
          )}
          {views.length === 0 && !scanning && !problem && (
            <PanelSectionRow>
              <Field focusable={false} label={t("hosts_empty_title")} description={t("decky_empty_body")} />
            </PanelSectionRow>
          )}
          {ready.map((v) => (
            <HostRow key={v.ref} host={v} showAddress={shared.has(v.name)} />
          ))}
          {access.length === 0 && refreshRow}
        </PanelSection>
      )}
      {access.length > 0 && (
        <PanelSection title={t("decky_host_needs_access")}>
          {access.map((v) => (
            <AccessRow key={v.ref} host={v} showAddress={shared.has(v.name)} refresh={refresh} />
          ))}
          {refreshRow}
        </PanelSection>
      )}
    </>
  );
};
