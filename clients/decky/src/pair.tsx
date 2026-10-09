// PIN pairing modal — a gamepad-navigable digit grid (the OSK is unreliable in Gaming Mode).
// The host displays the PIN after the operator arms pairing; the user enters it here.
import { DialogButton, Focusable, ModalRoot, Spinner } from "@decky/ui";
import { toaster } from "@decky/api";
import { FC, useState } from "react";
import { pair } from "./backend";
import type { HostView } from "./hosts";

/**
 * User-facing copy for a failed ceremony. The CLI's stable exit codes say WHICH failure it was,
 * so the keypad can name the fix instead of echoing a log line: `refused` is overwhelmingly a
 * mistyped PIN or a host nobody armed, and telling someone to check their network for that
 * would send them the wrong way entirely.
 */
function pairErrorBody(error: string | undefined, name: string): string {
  switch (error) {
    case "refused":
      return "Wrong PIN, or the host isn’t showing one. Arm pairing again and retry.";
    case "unreachable":
      return `Couldn’t reach ${name}`;
    case "client-outdated":
      return "Update the Punktfunk client to pair from here";
    case "client-unavailable":
      return "Couldn’t reach the Punktfunk client — is it still installed?";
    default:
      return "Pairing failed";
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
        toaster.toast({ title: "Punktfunk", body: `Paired with ${host.name}` });
        onPaired();
        closeModal?.();
      } else {
        setError(pairErrorBody(res.error, host.name));
        setPin("");
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <ModalRoot closeModal={closeModal}>
      <div style={{ fontWeight: "bold", fontSize: "1.3em", marginBottom: "0.3em" }}>
        Pair with {host.name}
      </div>
      <div style={{ opacity: 0.8, marginBottom: "1em" }}>
        Arm pairing on the host (its console or web UI), then enter the 4-digit PIN it shows.
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
          {busy ? <Spinner style={{ height: "1em" }} /> : "Pair"}
        </DialogButton>
      </Focusable>
    </ModalRoot>
  );
};
