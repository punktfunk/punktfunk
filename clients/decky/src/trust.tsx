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
import { toaster } from "@decky/api";
import { FC, useRef, useState } from "react";
import { requestAccess, trustHost } from "./backend";
import type { HostView } from "./hosts";
import { PairModal } from "./pair";

/** User-facing copy for a `trustHost` failure code. */
function trustErrorBody(error: string | undefined, name: string): string {
  switch (error) {
    case "refused":
      return `${name} is already saved under a different identity. Forget it in the Punktfunk app before trusting it again.`;
    case "client-outdated":
      return "Update the Punktfunk client to use request access";
    case "client-unavailable":
      return "Couldn’t reach the Punktfunk client — is it still installed?";
    default:
      return `Couldn’t save ${name}`;
  }
}

export const TrustSheet: FC<{
  host: HostView;
  closeModal?: () => void;
  /** Stream this host, having just been let in. */
  onStream: (opts: { requestAccess?: boolean }) => void;
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
        stream({});
        closeModal?.();
        return;
      }
      // Step 2: the request, pinned to that fingerprint. It parks until somebody answers.
      toaster.toast({
        title: "Punktfunk",
        body: `Approve this Deck in ${h.name}’s console`,
        duration: 10_000,
      });
      const res = await requestAccess(h.addr, h.port, "Steam Deck");
      if (!res.ok) {
        setError(
          res.error === "client-outdated"
            ? trustErrorBody(res.error, h.name)
            : (res.detail ?? `${h.name} didn’t let this Deck in`),
        );
        setBusy(false);
        return;
      }
      toaster.toast({ title: "Punktfunk", body: `Paired with ${h.name}` });
      changed();
      closeModal?.();
    } catch (e) {
      setError(String(e));
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
        Connect to {host.name}
      </div>
      <div style={{ opacity: 0.8, marginBottom: "1em" }}>
        {!hasIdentity
          ? "No advertised identity for this host — pair with a PIN instead."
          : canTrustDirectly
            ? `${host.name} accepts new devices. Connecting pins its identity so later streams are silent.`
            : `${host.name} needs to let this device in before it can stream.`}
      </div>
      {error && (
        <div style={{ color: "#ff6b6b", marginBottom: "0.6em" }}>{error}</div>
      )}

      <Focusable style={{ display: "flex", flexDirection: "column", gap: "0.5em" }}>
        {canRequestAccess && (
          <DialogButton disabled={busy} onClick={() => void letIn(true)}>
            {busy ? <Spinner style={{ height: "1em" }} /> : "Request access"}
          </DialogButton>
        )}
        {canTrustDirectly && (
          <DialogButton disabled={busy} onClick={() => void letIn(false)}>
            {busy ? <Spinner style={{ height: "1em" }} /> : "Connect"}
          </DialogButton>
        )}
        <DialogButton disabled={busy} onClick={usePin}>
          Use a PIN instead…
        </DialogButton>
        <DialogButton disabled={busy} onClick={() => closeModal?.()}>
          Cancel
        </DialogButton>
      </Focusable>

      {canRequestAccess && (
        <div style={{ opacity: 0.6, fontSize: "0.85em", marginTop: "0.8em" }}>
          Request access asks {host.name}’s operator to approve this Deck in its console or web
          UI. No PIN to type — this Deck is paired as soon as they do.
        </div>
      )}
    </ModalRoot>
  );
};
