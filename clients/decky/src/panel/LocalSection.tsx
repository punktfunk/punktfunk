// "Games in Steam": one switch per source of this device's own library, off until the player turns
// it on, the sync's progress, Remove for titles that left the library, and the titles that share a
// stream's controller layout. Shown only while this device's host answers and Steam owns its screen.
import { ButtonItem, Field, PanelSection, PanelSectionRow, ToggleField } from "@decky/ui";
import { FC, useEffect, useState } from "react";
import { t } from "../i18n";
import { useSignal } from "../lib/signal";
import { localEntryPairs } from "../steam/keys";
import { goneIds, sharedLayout, wanted } from "../steam/local-plan";
import {
  localState,
  panelOpened,
  requestSync,
  setSourceOn,
  sourcesOn,
  streamLayoutNames,
} from "../steam/local-sync";

export const LocalSection: FC = () => {
  const { answer, left } = useSignal(localState);
  const [on, setOn] = useState(sourcesOn);
  useEffect(() => {
    panelOpened();
  }, []);
  const gone = answer ? goneIds(answer, localEntryPairs() ?? []).length : 0;
  if (!answer || (answer.sources.length === 0 && gone === 0)) {
    return null;
  }
  const shared = sharedLayout(wanted(answer, on), streamLayoutNames());
  return (
    <PanelSection title={t("decky_local_title")}>
      {answer.sources.map((s) => (
        <PanelSectionRow key={s.id}>
          <ToggleField
            label={s.name}
            description={t("decky_local_source_hint")}
            checked={on.has(s.id)}
            onChange={(checked) => {
              setSourceOn(s.id, checked);
              setOn(sourcesOn());
            }}
          />
        </PanelSectionRow>
      ))}
      {left > 0 && (
        <PanelSectionRow>
          <Field focusable={false} description={t("decky_local_progress", { count: left })} />
        </PanelSectionRow>
      )}
      {gone > 0 && (
        <PanelSectionRow>
          <ButtonItem
            layout="below"
            description={t("decky_local_gone_hint", { count: gone })}
            onClick={() => requestSync("all")}
          >
            {t("common_remove")}
          </ButtonItem>
        </PanelSectionRow>
      )}
      {shared.map((title) => (
        <PanelSectionRow key={title}>
          <Field focusable={false} label={title} description={t("decky_local_shared_layout_hint")} />
        </PanelSectionRow>
      ))}
    </PanelSection>
  );
};
