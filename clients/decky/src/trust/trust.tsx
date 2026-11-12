// The trust sheet — the step between "I can see a host" and "I can stream it".
//
// Two ways in, in the order the GTK dialog and the console's pair screen offer them:
//
//   • REQUEST ACCESS (default) — no PIN. Save the host with the fingerprint it ADVERTISED,
//     then ask. The host parks the request until its operator approves this Deck in the
//     console or web UI; the Deck is then paired, and the next Connect streams.
//   • USE A PIN INSTEAD — the existing gamepad-navigable keypad (pair.tsx).
//
// NO FINGERPRINT, NO REQUEST ACCESS. The request pins the advertised fingerprint, and that pin
// is the only thing standing between a 185 s wait and an impostor answering for the host. A host typed in by address advertises nothing, so it gets the PIN path only — and is
// told why, rather than being shown a button that could only fail. Under no circumstances does
// this sheet trust-on-first-use its way past a missing fingerprint.
import { DialogButton, Focusable, ModalRoot, Spinner, showModal } from "@decky/ui";
import { FC, useRef, useState } from "react";
import { type PairResult, requestAccess, trustHost } from "../backend";
import { diag } from "../lib/diag";
import { toast } from "../lib/toast";
import type { HostView } from "../hosts/model";
import { rejectText, t } from "../i18n";
import { PairModal } from "./pair";

/** User-facing copy for a `trustHost` failure code. */
function trustErrorBody(error: string | undefined, name: string): string {
  switch (error) {
    case "refused":
      return t("decky_trust_refused", { host: name });
    case "client-outdated":
      return t("decky_trust_outdated");
    case "client-unavailable":
      return t("decky_client_unreachable");
    default:
      return t("decky_trust_save_failed", { host: name });
  }
}

/** User-facing copy for a failed `requestAccess`: the host's own reason when it gave one. */
function accessErrorBody(res: PairResult, name: string): string {
  switch (res.error) {
    case "client-outdated":
      return t("decky_trust_outdated");
    case "client-unavailable":
      return t("decky_client_unreachable");
    case "bad-fingerprint":
      return t("decky_trust_bad_fingerprint", { host: name });
    case "refused":
      return rejectText(res.reject) ?? t("decky_trust_denied", { host: name });
    default:
      return t("decky_trust_request_failed", { host: name });
  }
}

export const TrustSheet: FC<{
  host: HostView;
  closeModal?: () => void;
  /** Stream this host, having just been let in. */
  onStream: () => void;
  /** Re-read the host list — the record changed underneath the panel. */
  onChanged: () => void;
}> = ({ host, closeModal, onStream, onChanged }) => {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // ⚠ This sheet is a `showModal` PORTAL: it captures its callbacks ONCE and never re-renders
  // from panel state. Anything it needs to act on later must be read through a ref, not out of
  // a captured value — reading a captured array is exactly what made pinning a second game
  // compute from a stale base and clobber the first.
  const props = useRef({ host, onStream, onChanged });
  props.current = { host, onStream, onChanged };

  // Request access pins what the host ADVERTISES. The record's own pin is a different thing:
  // a host that already has one streams without ever opening this sheet.
  const hasIdentity = host.advertisedFp !== "";
  // A host advertising `pair=optional` admits anyone who pins its identity — there is no
  // operator decision to wait for, and asking for one would be a wait that never ends and a
  // record claiming somebody approved this Deck when nobody did. `paired` means the PIN
  // ceremony or a real approval; the desktop client records exactly this case as *trusted*.
  const needsApproval = host.pairPolicy !== "optional";
  const canRequestAccess = hasIdentity && needsApproval;
  const canTrustDirectly = hasIdentity && !needsApproval;

  /**
   * Pin the advertised identity, then get in.
   *
   * `approval` waits ~185 s for an operator and ends paired, with no stream. Without it the host
   * takes new devices, so the stream starts at once and the record stays trusted.
   */
  const letIn = async (approval: boolean) => {
    setBusy(true);
    setError(null);
    const { host: h, onStream: stream, onChanged: changed } = props.current;
    try {
      // Step 1: save it with the ADVERTISED fingerprint, pinned but unpaired ("trusted").
      // Idempotent, so a retry after a declined approval is free.
      const r = await trustHost(h.addr, h.port, h.advertisedFp, h.name);
      if (!r.ok) {
        setError(trustErrorBody(r.error, h.name));
        setBusy(false);
        return;
      }
      changed();
      if (!approval) {
        stream();
        closeModal?.();
        return;
      }
      // Step 2: the request, pinned to that fingerprint. It parks until somebody answers.
      toast(t("decky_trust_approve_toast", { host: h.name }), { duration: 10_000 });
      const res = await requestAccess(h.addr, h.port, "Steam Deck", h.advertisedFp);
      if (!res.ok) {
        diag(`request access: ${res.error} ${res.reject ?? ""} ${res.detail ?? ""}`);
        setError(accessErrorBody(res, h.name));
        setBusy(false);
        return;
      }
      toast(t("pair_paired", { host: h.name }));
      changed();
      closeModal?.();
    } catch (e) {
      diag(`request access threw: ${e}`);
      setError(t("decky_client_unreachable"));
      setBusy(false);
    }
  };

  const usePin = () => {
    // Hand off to the keypad. Closing first keeps one modal on screen at a time, which is what
    // the gamepad focus model expects.
    const { host: h, onChanged: changed } = props.current;
    closeModal?.();
    showModal(<PairModal host={h} onPaired={changed} />);
  };

  return (
    <ModalRoot closeModal={closeModal}>
      <div style={{ fontWeight: "bold", fontSize: "1.3em", marginBottom: "0.3em" }}>
        {t("decky_trust_title", { host: host.name })}
      </div>
      <div style={{ opacity: 0.8, marginBottom: "1em" }}>
        {!hasIdentity
          ? t("decky_trust_no_identity")
          : canTrustDirectly
            ? t("decky_trust_open_host", { host: host.name })
            : t("decky_trust_needs_approval", { host: host.name })}
      </div>
      {error && (
        <div style={{ color: "#ff6b6b", marginBottom: "0.6em" }}>{error}</div>
      )}

      <Focusable style={{ display: "flex", flexDirection: "column", gap: "0.5em" }}>
        {canRequestAccess && (
          <DialogButton disabled={busy} onClick={() => void letIn(true)}>
            {busy ? <Spinner style={{ height: "1em" }} /> : t("pair_request_access")}
          </DialogButton>
        )}
        {canTrustDirectly && (
          <DialogButton disabled={busy} onClick={() => void letIn(false)}>
            {busy ? <Spinner style={{ height: "1em" }} /> : t("common_connect")}
          </DialogButton>
        )}
        <DialogButton disabled={busy} onClick={usePin}>
          {t("decky_use_pin")}
        </DialogButton>
        <DialogButton disabled={busy} onClick={() => closeModal?.()}>
          {t("common_cancel")}
        </DialogButton>
      </Focusable>

      {canRequestAccess && (
        <div style={{ opacity: 0.6, fontSize: "0.85em", marginTop: "0.8em" }}>
          {t("decky_trust_request_hint", { host: host.name })}
        </div>
      )}
    </ModalRoot>
  );
};
