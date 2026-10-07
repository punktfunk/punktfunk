// Plugin entry: the Quick Access Menu panel. That is the whole plugin now — the fullscreen
// route, the settings screen, the host editor and the games picker are gone, because the
// client's own console home does all four one shortcut away (and is gamepad-navigable, which
// a QAM panel re-implementing them never quite was).
//
// What is left is what only a Decky plugin can do: start a stream through Steam so gamescope
// focuses it (see steam.ts), and stand in front of the trust decision that gates it.
import {
  ButtonItem,
  ConfirmModal,
  Field,
  Navigation,
  PanelSection,
  PanelSectionRow,
  Spinner,
  showModal,
  staticClasses,
  ToggleField,
} from "@decky/ui";
import { definePlugin, toaster } from "@decky/api";
import { FC, useEffect, useState } from "react";
import {
  FaDownload,
  FaGamepad,
  FaLock,
  FaPlay,
  FaPlus,
  FaStopCircle,
  FaSyncAlt,
  FaTv,
} from "react-icons/fa";
import {
  endGame,
  type HostProfile,
  hostAction,
  killStream,
  profiles as fetchProfiles,
  streamRunning,
} from "./backend";
import { PluginErrorBoundary } from "./boundary";
import {
  applyUpdate,
  checkForUpdatesNow,
  clientUpdateIsManualOnly,
  clientUpdateIsOneTap,
  hasUpdate,
  HostView,
  needsPair,
  refreshHosts,
  startStream,
  trustState,
  useHosts,
  useUpdate,
} from "./hooks";
import {
  gamePageStreamEnabled,
  installGamePageStream,
  setGamePageStreamEnabled,
} from "./library-page";
import { OsMark } from "./os-icon";
import {
  ensureGamepadUiShortcut,
  lastGameStream,
  launchGamepadUi,
  recreateShortcuts,
  removeGameShortcuts,
  stopStream,
  watchRunningStreams,
} from "./steam";
import { TrustSheet } from "./trust";

// Recovery action for "the Punktfunk library entry vanished" — recreates the visible shortcut
// and sweeps duplicate entries (the piles a boot race used to mint, one per Steam start).
// Deleting the shortcut (optionally + reinstalling the plugin) leaves a stale appId in Steam's
// CEF localStorage that self-heal fixes on the next mount, but this gives an in-session button
// that works even without a reload. Always ends in a toast so the tap has feedback.
async function recreatePunktfunkShortcut(): Promise<void> {
  const { appId, removedDuplicates } = await recreateShortcuts();
  toaster.toast({
    title: "Punktfunk",
    body:
      appId == null
        ? "Couldn't create the shortcut"
        : removedDuplicates > 0
          ? `Shortcut restored — removed ${removedDuplicates} duplicate ${
              removedDuplicates === 1 ? "entry" : "entries"
            }`
          : "Shortcut restored to your library",
  });
}

/** Delete the hidden per-game entries that game-page streams minted. Each comes back, with its
 *  art, on the next Stream tap for that game — so this is tidying, never a loss. */
function removeGamePageShortcuts(): void {
  const removed = removeGameShortcuts();
  toaster.toast({
    title: "Punktfunk",
    body:
      removed === 0
        ? "No game shortcuts to remove"
        : `Removed ${removed} game ${removed === 1 ? "shortcut" : "shortcuts"}`,
  });
}

/** Force-stop a wedged stream: end Steam's "game", then make sure the client itself is gone. */
async function forceStop(): Promise<void> {
  stopStream();
  try {
    await killStream();
  } catch {
    /* best-effort — the TerminateApp above is usually enough */
  }
  toaster.toast({ title: "Punktfunk", body: "Stopped the stream" });
}

// Press a host system button (guide/QAM) on the running stream, then hand the screen back
// to it — closing the local menus is what lets the HOST's overlay show through. The raw
// Steam/··· presses stay on the Deck by default (both overlays would open at once), so this
// is the panel route to the host's menus; holding Select is the controller route.
async function pressHost(action: "guide" | "qam"): Promise<void> {
  const r = await hostAction(action).catch(() => ({ ok: false as const, error: "backend" }));
  if (r.ok) {
    Navigation.CloseSideMenus();
  } else {
    toaster.toast({
      title: "Punktfunk",
      body: r.error === "no-stream" ? "No stream is running" : "Couldn't reach the stream",
    });
  }
}

/** End the title the last game-page stream launched, then the stream — only once the host says
 *  the game is gone. A refusal keeps the stream and shows the host's sentence. */
async function endStreamedGame(): Promise<void> {
  const last = lastGameStream();
  if (!last) {
    return;
  }
  const r = await endGame(last.ref, last.gameId).catch(() => ({
    ok: false as const,
    notice: "Couldn't reach the Punktfunk client.",
  }));
  if (r.ok) {
    stopStream();
  }
  toaster.toast({ title: "Punktfunk", body: r.notice || (r.ok ? `Ended ${last.title}.` : "Couldn't end the game.") });
}

/** The line under a host's name: where it is, whether it's up, and how far trust has got. */
function hostDescription(v: HostView): string {
  const trust = {
    paired: "paired",
    trusted: "trusted",
    "needs-access": "needs access",
  }[trustState(v)];
  return `${v.addr}:${v.port} · ${v.online ? "online" : "offline"} · ${trust}`;
}

/** The profiles of a paired, online host. Empty for a host with one profile or none: no chips. */
function useHostProfiles(host: HostView, gated: boolean): HostProfile[] {
  const [list, setList] = useState<HostProfile[]>([]);
  useEffect(() => {
    if (gated || !host.online) {
      setList([]);
      return;
    }
    let live = true;
    void fetchProfiles(host.ref)
      .then((r) => live && setList(r.ok && (r.profiles?.length ?? 0) > 1 ? r.profiles! : []))
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [host.ref, host.online, gated]);
  return list;
}

const HostRow: FC<{ host: HostView; refresh: () => void }> = ({ host, refresh }) => {
  const gated = needsPair(host);
  const people = useHostProfiles(host, gated);
  const stream = (opts: { requestAccess?: boolean } = {}) => void startStream(host, opts);
  return (
    <>
      <PanelSectionRow>
        <ButtonItem
          layout="below"
          onClick={() =>
            gated
              ? showModal(
                  <TrustSheet host={host} onStream={stream} onChanged={refresh} />,
                )
              : stream()
          }
          label={
            <span style={{ display: "inline-flex", alignItems: "center", gap: "0.4em" }}>
              {gated ? <FaLock /> : <OsMark os={host.os} />}
              {host.name}
            </span>
          }
          description={hostDescription(host)}
        >
          {gated ? "Connect…" : "Stream"}
        </ButtonItem>
      </PanelSectionRow>
      {/* Pinned cards, nested under their host rather than in a section of their own: a card
          IS a (host, preset) pair, and a row that floats free of its host is the "a pinned
          tile reads as a duplicate host" problem the desktop shells still have. The host's
          own BOUND preset is deliberately not a card — it applies silently on the plain row
          above, and showing it twice would suggest they do different things. */}
      {!gated &&
        host.pinnedPresets.map((p) => (
          <PanelSectionRow key={`${host.ref}:${p.id}`}>
            <ButtonItem
              layout="below"
              onClick={() => void startStream(host, { presetId: p.id }, `“${p.name}”`)}
              label={`▸ ${p.name}`}
            >
              <FaPlay style={{ marginRight: "0.5em" }} />
              Stream
            </ButtonItem>
          </PanelSectionRow>
        ))}
      {/* One chip per profile on a box with several: streams as that player, this time only. */}
      {people.map((p) => (
        <PanelSectionRow key={`${host.ref}:as:${p.id}`}>
          <ButtonItem
            layout="below"
            onClick={() => void startStream(host, { profileId: p.id }, p.display_name)}
            label={`● ${p.display_name}`}
            description={p.note ?? undefined}
          >
            <FaPlay style={{ marginRight: "0.5em" }} />
            Stream
          </ButtonItem>
        </PanelSectionRow>
      ))}
    </>
  );
};

const QamPanel: FC = () => {
  const { views, scanning, problem, refresh } = useHosts();
  const { info: update, checking, check } = useUpdate();
  // The host-buttons section shows only while the streaming client is up (checked per
  // panel open — the QAM panel mounts fresh each time).
  const [streaming, setStreaming] = useState(false);
  useEffect(() => {
    let live = true;
    void streamRunning()
      .then((r) => live && setStreaming(r.running))
      .catch(() => {});
    return () => {
      live = false;
    };
  }, []);
  const [gamePageStream, setGamePageStream] = useState(gamePageStreamEnabled);

  return (
    <>
      {hasUpdate(update) &&
        // A client this Deck can't install (a sysext, a nix profile, a source build, or a box
        // that hasn't opted into one-tap updates) gets the command, not a button — tapping
        // something that can only fail is worse than reading one line. A pending PLUGIN update
        // still wins the button, since that half always works.
        (clientUpdateIsManualOnly(update) && !update!.update_available ? (
          <PanelSection title="Client update available">
            <PanelSectionRow>
              <Field
                focusable
                label={update!.client_latest || "Newer version"}
                description={update!.client_opt_in || update!.client_command}
              />
            </PanelSectionRow>
          </PanelSection>
        ) : (
          <PanelSection title="Update available">
            <PanelSectionRow>
              <ButtonItem
                layout="below"
                onClick={() => applyUpdate(update!, check)}
                label={
                  update!.update_available
                    ? // "+ client" only when this tap will really install it. A manual-only
                      // client rides along as a toast with the command, and promising it in the
                      // label would make that read as a failure.
                      `Plugin v${update!.current} → v${update!.latest}${
                        clientUpdateIsOneTap(update) ? " + client" : ""
                      }`
                    : "New client version"
                }
                description="Installing can take a couple of minutes"
              >
                <FaDownload style={{ marginRight: "0.5em" }} />
                Update Punktfunk
              </ButtonItem>
            </PanelSectionRow>
          </PanelSection>
        ))}

      <PanelSection title="Hosts">
        <PanelSectionRow>
          <ButtonItem layout="below" onClick={() => void refresh()} disabled={scanning}>
            {scanning ? (
              <Spinner style={{ height: "1em", marginRight: "0.5em" }} />
            ) : (
              <FaSyncAlt style={{ marginRight: "0.5em" }} />
            )}
            {scanning ? "Scanning…" : "Refresh"}
          </ButtonItem>
        </PanelSectionRow>
        {/* A client that is missing or too old explains itself rather than rendering an empty
            list — "no hosts on your LAN" would blame the network for the plugin's problem, and
            for the outdated case the button that fixes it is in this same panel. */}
        {problem && (
          <PanelSectionRow>
            <Field
              focusable={false}
              label={
                problem === "client-unavailable"
                  ? "Punktfunk isn’t installed"
                  : problem === "list-failed"
                    ? "Couldn’t scan for hosts"
                    : "Update the Punktfunk client"
              }
              description={
                problem === "client-unavailable"
                  ? "This panel launches the Punktfunk app, which isn’t on this Deck yet. Install it in Desktop Mode."
                  : problem === "list-failed"
                    ? "Something went wrong while scanning — Refresh tries again."
                    : "This client is too old to find hosts on your network. Saved hosts still work."
              }
            />
          </PanelSectionRow>
        )}
        {views.length === 0 && scanning && (
          <PanelSectionRow>
            <Field focusable={false} description="Scanning your network…" />
          </PanelSectionRow>
        )}
        {views.length === 0 && !scanning && !problem && (
          <PanelSectionRow>
            <Field
              focusable={false}
              label="No hosts yet"
              description="Open Punktfunk to find and pair one."
            />
          </PanelSectionRow>
        )}
        {views.map((v) => (
          <HostRow key={v.ref} host={v} refresh={refresh} />
        ))}
      </PanelSection>

      <PanelSection title="Punktfunk">
        <PanelSectionRow>
          <ButtonItem
            layout="below"
            description="Settings, adding a host by address, and browsing a host's games all live here."
            onClick={() => void launchGamepadUi()}
          >
            <FaTv style={{ marginRight: "0.5em" }} />
            Open Punktfunk
          </ButtonItem>
        </PanelSectionRow>
        <PanelSectionRow>
          <ToggleField
            label="Punktfunk in Steam's Play menu"
            description="The ▾ beside a game's Play button lists your hosts that have it, next to Steam Link's; pick one and Play becomes Stream."
            checked={gamePageStream}
            onChange={(on) => {
              setGamePageStreamEnabled(on);
              setGamePageStream(on);
            }}
          />
        </PanelSectionRow>
      </PanelSection>

      {streaming && (
        <PanelSection title="Host menus">
          <PanelSectionRow>
            <ButtonItem
              layout="below"
              description="Press the Steam/guide button on the host"
              onClick={() => void pressHost("guide")}
            >
              <FaGamepad style={{ marginRight: "0.5em" }} />
              Steam menu on host
            </ButtonItem>
          </PanelSectionRow>
          <PanelSectionRow>
            <ButtonItem
              layout="below"
              description="Open the host's Quick Access Menu"
              onClick={() => void pressHost("qam")}
            >
              <FaGamepad style={{ marginRight: "0.5em" }} />
              Quick access on host
            </ButtonItem>
          </PanelSectionRow>
          {lastGameStream() && (
            <PanelSectionRow>
              <ButtonItem
                layout="below"
                description={`Close ${lastGameStream()?.title} on the host, then end the stream. Unsaved progress is lost.`}
                onClick={() =>
                  showModal(
                    <ConfirmModal
                      strTitle={`End ${lastGameStream()?.title}?`}
                      strDescription="Unsaved progress in the game is lost."
                      strOKButtonText="End game"
                      onOK={() => void endStreamedGame()}
                    />,
                  )
                }
              >
                <FaStopCircle style={{ marginRight: "0.5em" }} />
                End game
              </ButtonItem>
            </PanelSectionRow>
          )}
        </PanelSection>
      )}

      <PanelSection title="About">
        <PanelSectionRow>
          <Field
            focusable={false}
            label="Version"
            description={
              update
                ? `v${update.current}${update.channel ? ` · ${update.channel}` : " · dev build"}`
                : "…"
            }
          />
        </PanelSectionRow>
        <PanelSectionRow>
          <ButtonItem
            layout="below"
            disabled={checking}
            onClick={() => void checkForUpdatesNow(check)}
          >
            {checking ? "Checking…" : "Check for updates"}
          </ButtonItem>
        </PanelSectionRow>
        <PanelSectionRow>
          <ButtonItem
            layout="below"
            description="Missing the Punktfunk entry in your library, or seeing several? This puts one back and removes the rest."
            onClick={() => void recreatePunktfunkShortcut()}
          >
            <FaPlus style={{ marginRight: "0.5em" }} />
            Recreate library shortcut
          </ButtonItem>
        </PanelSectionRow>
        <PanelSectionRow>
          <ButtonItem
            layout="below"
            description="Streams started from a game's page run under a hidden entry named after that game. This removes them; each returns on its next Stream."
            onClick={() => removeGamePageShortcuts()}
          >
            Remove game shortcuts
          </ButtonItem>
        </PanelSectionRow>
        <PanelSectionRow>
          <ButtonItem
            layout="below"
            description="Ends a stream that stopped responding."
            onClick={() => void forceStop()}
          >
            <FaStopCircle style={{ marginRight: "0.5em" }} />
            Force-stop
          </ButtonItem>
        </PanelSectionRow>
      </PanelSection>
    </>
  );
};

export default definePlugin(() => {
  // Ensure the visible, stateless "Punktfunk" library entry (opens the gamepad UI / console
  // home) exists and is repointed to the current plugin dir — also installs the native-touch
  // controller config. Fire-and-forget: cosmetic library upkeep must never block plugin load.
  void ensureGamepadUiShortcut();
  // Warm the host list and each paired host's library now, so the first game page opened
  // already knows which titles a host can stream — the QAM panel may never have been opened.
  void refreshHosts();
  // The Stream button on Steam's game pages (see library-page.tsx). Removed on dismount, or
  // Steam keeps calling into a plugin that is gone.
  const removeGamePageStream = installGamePageStream();
  // Steam's app lifetime feed tells the game page when its stream is up (Stream ↔ Stop).
  const unwatchRunning = watchRunningStreams();
  return {
    onDismount() {
      removeGamePageStream();
      unwatchRunning();
    },
    // `name` must stay in sync with plugin.json (the loader keys plugins by it) — and it is
    // USER-VISIBLE: Decky labels the entry in its plugin list with it, so it carries the brand
    // case. Decky finds an installed plugin by matching plugin.json "name" (never the folder
    // name), so this is independent of the on-disk dir, which stays lowercase `punktfunk`.
    name: "Punktfunk",
    // `staticClasses?.Title` is guarded so a future client that drops the export can't throw
    // at plugin-load time (an error boundary only catches render-time, not load-time, errors).
    titleView: <div className={staticClasses?.Title}>Punktfunk</div>,
    content: (
      <PluginErrorBoundary>
        <QamPanel />
      </PluginErrorBoundary>
    ),
    icon: <FaTv />,
  };
});
