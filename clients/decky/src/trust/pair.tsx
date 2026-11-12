// PIN pairing modal — a gamepad-navigable digit grid (the OSK is unreliable in Gaming Mode).
// The host displays the PIN after the operator arms pairing; the user enters it here.
import { DialogButton, Focusable, ModalRoot, Spinner } from "@decky/ui";
import { toaster } from "@decky/api";
import { FC, useState } from "react";
import { pair } from "../backend";
import { diag } from "../lib/diag";
import type { HostView } from "../hosts/model";
import { rejectText, t } from "../i18n";

/**
 * User-facing copy for a failed ceremony. The CLI's stable exit codes say WHICH failure it was,
 * so the keypad can name the fix instead of echoing a log line: `refused` is overwhelmingly a
 * mistyped PIN or a host nobody armed, and telling someone to check their network for that
 * would send them the wrong way entirely.
 */
function pairErrorBody(error: string | undefined, reject: string | undefined, name: string): string {
  switch (error) {
    case "refused":
      // The host's own reason, when it gave one; else the likeliest cause.
      return rejectText(reject) ?? t("decky_pair_wrong_pin");
    case "unreachable":
      return t("decky_pair_unreachable", { host: name });
    case "client-outdated":
      return t("decky_pair_outdated");
    case "client-unavailable":
      return t("decky_client_unreachable");
    default:
      return t("decky_pair_failed");
  }
}

export const PairModal: FC<{
  host: HostView;
  closeModal?: () => void;
  onPaired: () => void;
}> = ({ host, closeModal, onPaired }) => {
  const [pin, setPin] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const press = (d: string) => setPin((p) => (p.length >= 4 ? p : p + d));
  const back = () => setPin((p) => p.slice(0, -1));

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const res = await pair(host.addr, host.port, pin, "Steam Deck");
      if (res.ok) {
        toaster.toast({ title: "Punktfunk", body: t("pair_paired", { host: host.name }) });
        onPaired();
        closeModal?.();
      } else {
        diag(`pair: ${res.error} ${res.reject ?? ""} ${res.detail ?? ""}`);
        setError(pairErrorBody(res.error, res.reject, host.name));
        setPin("");
      }
    } catch (e) {
      diag(`pair threw: ${e}`);
      setError(t("decky_client_unreachable"));
    } finally {
      setBusy(false);
    }
  };

  return (
    <ModalRoot closeModal={closeModal}>
      <div style={{ fontWeight: "bold", fontSize: "1.3em", marginBottom: "0.3em" }}>
        {t("pair_title", { host: host.name })}
      </div>
      <div style={{ opacity: 0.8, marginBottom: "1em" }}>
        {t("decky_pair_intro")}
      </div>

      <div
        style={{
          fontSize: "2.2em",
          letterSpacing: "0.4em",
          textAlign: "center",
          fontFamily: "monospace",
          minHeight: "1.4em",
          marginBottom: "0.6em",
        }}
      >
        {pin.padEnd(4, "•")}
      </div>
      {error && (
        <div style={{ color: "#ff6b6b", textAlign: "center", marginBottom: "0.6em" }}>
          {error}
        </div>
      )}

      <Focusable
        style={{
          display: "grid",
          gridTemplateColumns: "repeat(3, 1fr)",
          gap: "0.5em",
        }}
      >
        {["1", "2", "3", "4", "5", "6", "7", "8", "9"].map((d) => (
          <DialogButton key={d} disabled={busy} onClick={() => press(d)}>
            {d}
          </DialogButton>
        ))}
        <DialogButton disabled={busy} onClick={back}>
          ⌫
        </DialogButton>
        <DialogButton disabled={busy} onClick={() => press("0")}>
          0
        </DialogButton>
        <DialogButton disabled={busy || pin.length !== 4} onClick={submit}>
          {busy ? <Spinner style={{ height: "1em" }} /> : t("pair_action")}
        </DialogButton>
      </Focusable>
    </ModalRoot>
  );
};
