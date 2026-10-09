//! Host update check and apply for the console (`design/host-update-from-web-console.md`).
//!
//! Fetches the per-channel signed manifest and verifies it against the Ed25519
//! keys pinned in `pf-update-check`. `GET /update/status` returns a process-wide
//! cache and, when older than [`AUTO_REFRESH_AFTER`], kicks a background refresh —
//! the console already polls, so this module has no timer of its own.
//! `POST /update/check` forces a refresh, rate-limited to one per [`FORCE_MIN_INTERVAL`].
//!
//! Apply takes no version, URL, or channel from the request; those come from the
//! verified cache. Trust and failure rules live in [`manifest`]. The serial floor
//! in `update-state.json` makes a replayed older manifest an error, not a silent
//! downgrade. `PUNKTFUNK_UPDATE_CHECK=0` disables network activity; status then
//! reports `check_disabled`. `PUNKTFUNK_UPDATE_APPLY=0` leaves check intact and
//! 409s apply.

pub(crate) mod detect;
pub(crate) mod jobs;
#[cfg(target_os = "linux")]
mod linux;
// Schema and validation live in `pf-update-check` (same crate the client uses).
pub(crate) use pf_update_check::manifest;
#[cfg(target_os = "windows")]
pub(crate) mod windows;

use manifest::Manifest;
use pf_update_check::{floor, FeedError};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 6 h: long enough not to hammer the signed feed; status polls kick refresh.
const AUTO_REFRESH_AFTER: Duration = Duration::from_secs(6 * 60 * 60);

/// 30 s: operator mash of `POST /update/check` must not stampede the feed.
pub(crate) const FORCE_MIN_INTERVAL: Duration = Duration::from_secs(30);

/// 45 d: freeze-detection hint in status, not an error. Serial is publish time.
const STALE_AFTER: Duration = Duration::from_secs(45 * 24 * 60 * 60);

pub(crate) fn check_disabled() -> bool {
    !pf_host_config::row_bool("PUNKTFUNK_UPDATE_CHECK")
}

/// Operator kill switch: apply 409s and status reports `notify` even when a
/// one-click leg exists. Check is unaffected.
pub(crate) fn apply_disabled() -> bool {
    !pf_host_config::row_bool("PUNKTFUNK_UPDATE_APPLY")
}

/// What an apply can use on this box, probed once so [`apply_leg`] is a pure table.
#[derive(Debug, Clone, Copy, Default)]
struct Caps {
    apply_disabled: bool,
    /// The only OS with helper and source-rebuild legs.
    linux: bool,
    /// Omarchy owns `pacman`: a one-click apply would hit its `pacman -Syu` guard or skip the
    /// snapper snapshot. Packages ride its transaction once the repo is configured.
    omarchy: bool,
    /// The packaged root helper's unit exists.
    helper: bool,
    /// The operator is in `punktfunk-update`, which polkit checks.
    opted_in: bool,
    /// Pacman's root-owned full-sysupgrade opt-in; the helper refuses pacman without it.
    pacman_optin: bool,
}

impl Caps {
    fn probe() -> Self {
        #[cfg(target_os = "linux")]
        {
            let helper = linux::helper_installed();
            Self {
                apply_disabled: apply_disabled(),
                linux: true,
                omarchy: crate::osinfo::is_omarchy(),
                helper,
                // Both shell out or read root-owned config, and neither matters without a helper.
                opted_in: helper && linux::opted_in(),
                pacman_optin: helper && linux::pacman_opted_in(),
            }
        }
        #[cfg(not(target_os = "linux"))]
        Self {
            apply_disabled: apply_disabled(),
            ..Self::default()
        }
    }
}

/// How [`start_apply`] installs a newer build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Leg {
    /// Download, verify and run the Windows installer.
    Installer,
    /// Start the `pf-update` root oneshot.
    Helper,
    /// Rebuild the Deck's own checkout. User-owned: no helper, no group.
    SteamosSource,
}

/// Kinds the root helper applies.
fn helper_kind(kind: detect::InstallKind) -> bool {
    use detect::InstallKind as K;
    matches!(kind, K::Apt | K::Dnf | K::Sysext | K::RpmOstree | K::Pacman)
}

/// The leg [`start_apply`] may run, or `None` where the console shows the command instead.
/// Group membership is not checked here: polkit enforces it when the helper starts.
fn apply_leg(kind: detect::InstallKind, c: Caps) -> Option<Leg> {
    use detect::InstallKind as K;
    if c.apply_disabled || c.omarchy {
        return None;
    }
    match kind {
        K::WindowsInstaller => Some(Leg::Installer),
        K::SteamosSource if c.linux => Some(Leg::SteamosSource),
        K::Pacman if !c.pacman_optin => None,
        k if helper_kind(k) && c.helper => Some(Leg::Helper),
        _ => None,
    }
}

/// `full` (one-click), `staged` (apply then reboot, rpm-ostree) or `notify` (show the command).
/// A helper leg also needs the operator's group before the console offers it.
fn support(kind: detect::InstallKind, c: Caps) -> &'static str {
    match apply_leg(kind, c) {
        Some(Leg::Helper) if !c.opted_in => "notify",
        Some(Leg::Helper) if kind == detect::InstallKind::RpmOstree => "staged",
        Some(_) => "full",
        None => "notify",
    }
}

/// Joining `punktfunk-update` would turn the command into a button.
fn opt_in_would_help(kind: detect::InstallKind, c: Caps) -> bool {
    !c.apply_disabled && !c.omarchy && c.helper && !c.opted_in && helper_kind(kind)
}

/// Shown instead of an Apply button when joining the group would enable one.
pub(crate) const OPT_IN_HINT: &str =
    "sudo usermod -aG punktfunk-update $USER   # enables web-triggered updates for this host";

pub(crate) fn apply_support() -> &'static str {
    support(detect::detect().0, Caps::probe())
}

/// Status copy when the helper is installed but the operator is not in `punktfunk-update`.
pub(crate) fn opt_in_hint() -> Option<String> {
    opt_in_would_help(detect::detect().0, Caps::probe()).then(|| OPT_IN_HINT.to_string())
}

#[derive(Clone)]
pub(crate) struct Checked {
    pub manifest: Manifest,
    pub fetched_unix: u64,
}

#[derive(Default)]
struct Runtime {
    checked: Option<Checked>,
    last_error: Option<String>,
    /// Empty feed, never seen a manifest. Kept out of `last_error` so status
    /// does not paint "nothing published yet" as a broken host.
    not_published: bool,
    /// At most one background refresh at a time.
    refreshing: bool,
    last_forced: Option<Instant>,
    /// Any attempt (forced or auto); drives [`AUTO_REFRESH_AFTER`].
    last_attempt: Option<Instant>,
    /// Version already emitted as `update.available`; a still-newer cache must
    /// not re-announce every auto-refresh.
    announced: Option<String>,
    /// Commits `~/punktfunk` is behind its upstream — the source build's only
    /// "newer" signal. Kept across a failed fetch; see [`source_newer`].
    source_behind: Option<u64>,
    job: Option<jobs::JobSnapshot>,
}

fn runtime() -> &'static Mutex<Runtime> {
    static RT: OnceLock<Mutex<Runtime>> = OnceLock::new();
    RT.get_or_init(|| Mutex::new(Runtime::default()))
}

/// The [`floor`] file: highest accepted manifest serial per channel.
fn state_path() -> PathBuf {
    pf_paths::config_dir().join("update-state.json")
}

/// The floor's write: [`pf_paths::replace_file`], else in place, since an unraised floor lets a
/// replayed older manifest through.
fn write_floor(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    pf_paths::replace_file(path, bytes).or_else(|_| std::fs::write(path, bytes))
}

/// Is a newer build out, for an install with no published artifact to compare?
///
/// A checkout's version says nothing about the remote, so the count of upstream
/// commits is the whole answer. An unanswered fetch is "nothing to show", never
/// "up to date" — a Deck must not be told it is current on a guess.
pub(crate) fn source_newer(behind: Option<u64>) -> bool {
    behind.is_some_and(|n| n > 0)
}

/// Does `manifest` offer an update to an install of `kind` running `current`? Status, the
/// `update.available` event and apply all ask this, so apply never refuses an offer.
/// A source build answers from its checkout: its version names a commit, not a published build.
pub(crate) fn offers_update(
    kind: detect::InstallKind,
    channel: detect::Channel,
    current: &str,
    manifest: &Manifest,
    source_behind: Option<u64>,
) -> bool {
    if kind == detect::InstallKind::SteamosSource {
        source_newer(source_behind)
    } else {
        detect::is_newer(
            &manifest.version,
            manifest.ci_run,
            manifest.commit.as_deref(),
            current,
            channel,
        )
    }
}

/// Blocking feed fetch; call from a blocking thread.
fn fetch_manifest_blocking(channel: &str) -> Result<Manifest, FeedError> {
    pf_update_check::feed::fetch_manifest_blocking(
        &pf_update_check::feed::feed_base(),
        channel,
        &pf_update_check::pinned_keys(),
        &format!("punktfunk-host/{} (update-check)", crate::version::get()),
    )
}

pub(crate) fn refresh_blocking() -> Result<Checked, FeedError> {
    let (kind, channel) = detect::detect();
    // Before the lock: this talks to the network. The manifest still fetches for the
    // "latest published" row, but it cannot decide a source build's answer.
    #[cfg(target_os = "linux")]
    let behind = (kind == detect::InstallKind::SteamosSource)
        .then(linux::source_behind)
        .flatten();
    #[cfg(not(target_os = "linux"))]
    let behind: Option<u64> = None;
    let result = fetch_manifest_blocking(channel.as_str()).and_then(|m| {
        let path = state_path();
        floor::check(&path, channel.as_str(), m.serial).map_err(FeedError::Failed)?;
        if let Err(e) = floor::raise(&path, channel.as_str(), m.serial, write_floor) {
            tracing::warn!(path = %path.display(), error = %e, "update serial floor not raised");
        }
        Ok(m)
    });

    let mut rt = runtime().lock().unwrap();
    rt.last_attempt = Some(Instant::now());
    rt.refreshing = false;
    if behind.is_some() {
        rt.source_behind = behind;
    }
    match result {
        Ok(m) => {
            let checked = Checked {
                manifest: m,
                fetched_unix: crate::clock::unix_secs_u64(),
            };
            let newer = offers_update(
                kind,
                channel,
                crate::version::get(),
                &checked.manifest,
                rt.source_behind,
            );
            if newer && rt.announced.as_deref() != Some(checked.manifest.version.as_str()) {
                rt.announced = Some(checked.manifest.version.clone());
                crate::events::emit(crate::events::EventKind::UpdateAvailable {
                    version: checked.manifest.version.clone(),
                    channel: channel.as_str().to_string(),
                    install_kind: kind.as_str().to_string(),
                });
            }
            rt.last_error = None;
            rt.not_published = false;
            rt.checked = Some(checked.clone());
            Ok(checked)
        }
        Err(e) => {
            let (last_error, not_published) = classify_failure(&e, rt.checked.is_some());
            rt.last_error = last_error;
            rt.not_published = not_published;
            Err(e)
        }
    }
}

/// `(last_error, not_published)` — never both. A 404 is benign only until a
/// manifest has been seen; after that the feed lost a document and must stay
/// a loud error.
fn classify_failure(e: &FeedError, had_manifest: bool) -> (Option<String>, bool) {
    if e.is_not_published() && !had_manifest {
        (None, true)
    } else {
        (Some(e.to_string()), false)
    }
}

/// Cache + errors; kick a background refresh when older than [`AUTO_REFRESH_AFTER`].
pub(crate) fn snapshot_and_maybe_refresh() -> Snapshot {
    let mut kick = false;
    let snap = {
        let mut rt = runtime().lock().unwrap();
        let cold = rt
            .last_attempt
            .map(|t| t.elapsed() >= AUTO_REFRESH_AFTER)
            .unwrap_or(true);
        if cold && !rt.refreshing && !check_disabled() {
            rt.refreshing = true;
            rt.last_attempt = Some(Instant::now());
            kick = true;
        }
        Snapshot {
            checked: rt.checked.clone(),
            last_error: rt.last_error.clone(),
            not_published: rt.not_published,
            job: rt.job.clone(),
            last_result: jobs::read_result(&jobs::result_path()),
            source_behind: rt.source_behind,
        }
    };
    if kick {
        // Outcome lands in the cache; the console's next poll reads it.
        tokio::task::spawn_blocking(|| {
            let _ = refresh_blocking();
        });
    }
    snap
}

/// Rate-limited `POST /update/check`. Blocks until the refresh finishes.
pub(crate) async fn force_check() -> Result<Snapshot, ForceError> {
    if check_disabled() {
        return Err(ForceError::Disabled);
    }
    {
        let mut rt = runtime().lock().unwrap();
        if let Some(t) = rt.last_forced {
            if t.elapsed() < FORCE_MIN_INTERVAL {
                return Err(ForceError::TooSoon);
            }
        }
        rt.last_forced = Some(Instant::now());
        rt.refreshing = true;
    }
    let _ = tokio::task::spawn_blocking(refresh_blocking).await;
    let rt = runtime().lock().unwrap();
    Ok(Snapshot {
        checked: rt.checked.clone(),
        last_error: rt.last_error.clone(),
        not_published: rt.not_published,
        job: rt.job.clone(),
        last_result: jobs::read_result(&jobs::result_path()),
        source_behind: rt.source_behind,
    })
}

pub(crate) enum ForceError {
    Disabled,
    TooSoon,
}

/// Refusal mapped to HTTP 409 by the API layer.
pub(crate) enum ApplyError {
    /// No one-click leg; the console shows the command instead.
    Unsupported,
    Disabled,
    /// In-process job, or a spawned installer that has not resolved yet.
    JobRunning,
    /// A stream is live and the request did not pass `force`.
    SessionActive,
    /// No verified newer manifest, or the Windows installer asset is missing.
    NothingToApply,
}

/// Apply from the verified cache only. The request carries no version, URL,
/// or channel.
pub(crate) fn start_apply(force: bool, session_active: bool) -> Result<(), ApplyError> {
    if apply_disabled() {
        return Err(ApplyError::Disabled);
    }
    let (kind, channel) = detect::detect();
    // The table [`apply_support`] reads, so a direct POST meets the same refusals.
    let leg = apply_leg(kind, Caps::probe()).ok_or(ApplyError::Unsupported)?;
    let windows_leg = leg == Leg::Installer;
    if session_active && !force {
        return Err(ApplyError::SessionActive);
    }

    let (target_version, serial, asset) = {
        let mut rt = runtime().lock().unwrap();
        if rt.job.is_some() {
            return Err(ApplyError::JobRunning);
        }
        // Fresh intent + old version is still an apply in flight. Reconcile
        // owns it; do not start a second job under it.
        if matches!(
            jobs::reconcile(
                jobs::read_intent(&jobs::intent_path()),
                crate::version::get(),
                crate::clock::unix_secs_u64()
            ),
            jobs::Reconciled::StillApplying
        ) {
            return Err(ApplyError::JobRunning);
        }
        let Some(checked) = rt.checked.as_ref() else {
            return Err(ApplyError::NothingToApply);
        };
        let newer = offers_update(
            kind,
            channel,
            crate::version::get(),
            &checked.manifest,
            rt.source_behind,
        );
        if !newer {
            return Err(ApplyError::NothingToApply);
        }
        // Linux legs resolve artifacts through the package manager. An ARM64 host reads only
        // its own key: the x64 asset is the wrong exe, not a fallback.
        let asset = if cfg!(target_arch = "aarch64") {
            checked.manifest.windows_host_arm64.clone()
        } else {
            checked.manifest.windows_host.clone()
        };
        if windows_leg && asset.is_none() {
            return Err(ApplyError::NothingToApply);
        }
        let version = checked.manifest.version.clone();
        let serial = checked.manifest.serial;
        rt.job = Some(jobs::JobSnapshot {
            target_version: version.clone(),
            stage: if windows_leg {
                "downloading"
            } else {
                "applying"
            },
            received_bytes: 0,
            total_bytes: None,
            started_unix: crate::clock::unix_secs_u64(),
        });
        (version, serial, asset)
    };

    tokio::task::spawn_blocking(move || {
        let stage = |s: &'static str| {
            let mut rt = runtime().lock().unwrap();
            if let Some(job) = rt.job.as_mut() {
                job.stage = s;
            }
        };
        let outcome: Result<PostApply, (&'static str, String)> = {
            #[cfg(target_os = "windows")]
            {
                let progress = |received: u64, total: Option<u64>| {
                    let mut rt = runtime().lock().unwrap();
                    if let Some(job) = rt.job.as_mut() {
                        job.received_bytes = received;
                        job.total_bytes = total;
                    }
                };
                let asset = asset.expect("windows leg reserved with an asset");
                windows::run_apply(&asset, &target_version, serial, &progress, &stage)
                    .map(|()| PostApply::AwaitRestart)
            }
            #[cfg(target_os = "linux")]
            {
                let _ = &asset; // unused: Linux legs use the package manager
                let run = if leg == Leg::SteamosSource {
                    linux::run_apply_steamos(&target_version, serial, &stage)
                } else {
                    linux::run_apply(&target_version, serial, &stage)
                };
                run.map(|()| {
                    // Staged / nothing-to-do wrote a durable result; in-place
                    // wrote the intent and queued restart. Either way the
                    // in-process job is finished.
                    PostApply::Done
                })
            }
            #[cfg(not(any(target_os = "windows", target_os = "linux")))]
            {
                let _ = (&asset, &target_version, serial, &stage);
                Err(("applying", "no apply leg for this platform".to_string()))
            }
        };
        match outcome {
            Ok(PostApply::AwaitRestart) => {
                // Leave stage `restarting`. The installer is about to kill
                // this process; boot reconcile writes the durable outcome.
            }
            Ok(PostApply::Done) => {
                runtime().lock().unwrap().job = None;
            }
            Err((stage_name, error)) => {
                let record = jobs::ResultRecord {
                    ok: false,
                    from: crate::version::get().into(),
                    to: target_version.clone(),
                    finished_unix: crate::clock::unix_secs_u64(),
                    stage: Some(stage_name.into()),
                    error: Some(error),
                    log_path: None,
                    staged: false,
                };
                let _ = jobs::write_json_atomic(&jobs::result_path(), &record);
                runtime().lock().unwrap().job = None;
            }
        }
    });
    Ok(())
}

/// What an apply leg leaves for the spawn wrapper. Each platform constructs
/// only its own variant; the other is matched but never built there.
#[allow(dead_code)]
enum PostApply {
    /// Process is about to die; reconcile owns the outcome.
    AwaitRestart,
    /// Finished in-process (staged / nothing-to-do); clear the job.
    Done,
}

/// Close an intent left by a previous apply. Call once from `mgmt::run`
/// before the API serves.
pub(crate) fn reconcile_at_boot() {
    let path = jobs::intent_path();
    let intent = jobs::read_intent(&path);
    match jobs::reconcile(intent, crate::version::get(), crate::clock::unix_secs_u64()) {
        jobs::Reconciled::None | jobs::Reconciled::StillApplying => {}
        jobs::Reconciled::Success(record) => {
            tracing::info!(from = %record.from, to = %record.to, "host update applied");
            let _ = jobs::write_json_atomic(&jobs::result_path(), &record);
            let _ = std::fs::remove_file(&path);
            crate::events::emit(crate::events::EventKind::UpdateApplied {
                from: record.from,
                to: record.to,
            });
        }
        jobs::Reconciled::Failed(record) => {
            tracing::warn!(
                from = %record.from,
                to = %record.to,
                error = record.error.as_deref().unwrap_or(""),
                "host update did NOT stick"
            );
            let _ = jobs::write_json_atomic(&jobs::result_path(), &record);
            let _ = std::fs::remove_file(&path);
        }
    }
}

pub(crate) struct Snapshot {
    pub checked: Option<Checked>,
    pub last_error: Option<String>,
    /// No release on this channel yet. Mutually exclusive with `last_error`.
    pub not_published: bool,
    /// Live in-process job. Mid-apply restart leaves this `None` while a
    /// fresh intent is still in flight — see [`Snapshot::applying_from_intent`].
    pub job: Option<jobs::JobSnapshot>,
    pub last_result: Option<jobs::ResultRecord>,
    /// Source builds only: commits behind upstream, `None` when git could not answer.
    pub source_behind: Option<u64>,
}

impl Snapshot {
    /// Apply in flight with no in-process job: a spawn that has not resolved
    /// (this process may be the old host in its last seconds, or a restart
    /// inside the grace window). The API surfaces it as a `restarting` job.
    pub(crate) fn applying_from_intent(&self) -> Option<jobs::IntentRecord> {
        if self.job.is_some() {
            return None;
        }
        let intent = jobs::read_intent(&jobs::intent_path())?;
        match jobs::reconcile(
            Some(intent.clone()),
            crate::version::get(),
            crate::clock::unix_secs_u64(),
        ) {
            jobs::Reconciled::StillApplying => Some(intent),
            _ => None,
        }
    }

    /// Last check succeeded but the manifest's publish serial is older than
    /// [`STALE_AFTER`] (freeze detection).
    pub(crate) fn stale(&self) -> bool {
        self.checked
            .as_ref()
            .map(|c| {
                crate::clock::unix_secs_u64().saturating_sub(c.manifest.serial)
                    > STALE_AFTER.as_secs()
            })
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp file that cannot be written beside the floor still raises it.
    #[test]
    fn floor_rises_when_the_temp_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        // A 255-byte name fits, but its temp name does not: the temp write fails, as a failed
        // rename would.
        let path = dir.path().join(format!("{}.json", "f".repeat(250)));
        floor::raise(&path, "stable", 5, write_floor).unwrap();
        floor::raise(&path, "stable", 9, write_floor).unwrap();
        assert_eq!(floor::load(&path, "stable"), 9);
        let files = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(files, 1, "no temp is left behind");
    }

    fn ready() -> Caps {
        Caps {
            apply_disabled: false,
            linux: true,
            omarchy: false,
            helper: true,
            opted_in: true,
            pacman_optin: true,
        }
    }

    const KINDS: [detect::InstallKind; 10] = {
        use detect::InstallKind as K;
        [
            K::WindowsInstaller,
            K::Flatpak,
            K::Sysext,
            K::RpmOstree,
            K::Apt,
            K::Dnf,
            K::Pacman,
            K::SteamosSource,
            K::Nix,
            K::Source,
        ]
    };

    /// The kill switch and Omarchy refuse every kind, in status and in apply alike.
    #[test]
    fn kill_switch_and_omarchy_refuse_every_leg() {
        for caps in [
            Caps {
                apply_disabled: true,
                ..ready()
            },
            Caps {
                omarchy: true,
                ..ready()
            },
        ] {
            for kind in KINDS {
                assert_eq!(apply_leg(kind, caps), None, "{}", kind.as_str());
                assert_eq!(support(kind, caps), "notify", "{}", kind.as_str());
                assert!(!opt_in_would_help(
                    kind,
                    Caps {
                        opted_in: false,
                        ..caps
                    }
                ));
            }
        }
    }

    #[test]
    fn each_kind_routes_to_its_leg() {
        use detect::InstallKind as K;
        for (kind, leg, tier) in [
            (K::WindowsInstaller, Some(Leg::Installer), "full"),
            (K::Apt, Some(Leg::Helper), "full"),
            (K::Dnf, Some(Leg::Helper), "full"),
            (K::Sysext, Some(Leg::Helper), "full"),
            (K::Pacman, Some(Leg::Helper), "full"),
            (K::RpmOstree, Some(Leg::Helper), "staged"),
            (K::SteamosSource, Some(Leg::SteamosSource), "full"),
            (K::Flatpak, None, "notify"),
            (K::Nix, None, "notify"),
            (K::Source, None, "notify"),
        ] {
            assert_eq!(apply_leg(kind, ready()), leg, "{}", kind.as_str());
            assert_eq!(support(kind, ready()), tier, "{}", kind.as_str());
        }
    }

    /// No helper, no helper leg. The Deck's source rebuild needs neither helper nor group.
    #[test]
    fn helper_legs_need_the_helper_and_pacman_its_opt_in() {
        use detect::InstallKind as K;
        let bare = Caps {
            helper: false,
            opted_in: false,
            pacman_optin: false,
            ..ready()
        };
        for kind in [K::Apt, K::Dnf, K::Sysext, K::RpmOstree, K::Pacman] {
            assert_eq!(apply_leg(kind, bare), None, "{}", kind.as_str());
        }
        assert_eq!(apply_leg(K::SteamosSource, bare), Some(Leg::SteamosSource));
        let no_sysupgrade = Caps {
            pacman_optin: false,
            ..ready()
        };
        assert_eq!(apply_leg(K::Pacman, no_sysupgrade), None);
        assert_eq!(apply_leg(K::Apt, no_sysupgrade), Some(Leg::Helper));
    }

    /// Apply runs a helper leg without the group, since polkit decides; status shows the
    /// command and the hint until the operator joins.
    #[test]
    fn the_group_gates_the_button_and_the_hint_only() {
        use detect::InstallKind as K;
        let outside = Caps {
            opted_in: false,
            ..ready()
        };
        assert_eq!(apply_leg(K::Apt, outside), Some(Leg::Helper));
        assert_eq!(support(K::Apt, outside), "notify");
        assert!(opt_in_would_help(K::Apt, outside));
        assert!(opt_in_would_help(
            K::Pacman,
            Caps {
                pacman_optin: false,
                ..outside
            }
        ));
        assert!(!opt_in_would_help(K::SteamosSource, outside));
        assert!(!opt_in_would_help(K::Apt, ready()));
        assert!(!opt_in_would_help(
            K::Apt,
            Caps {
                helper: false,
                ..outside
            }
        ));
    }

    /// Source-rebuild and helper legs are Linux-only.
    #[test]
    fn off_linux_only_the_installer_applies() {
        let elsewhere = Caps {
            apply_disabled: false,
            ..Caps::default()
        };
        for kind in KINDS {
            let want = (kind == detect::InstallKind::WindowsInstaller).then_some(Leg::Installer);
            assert_eq!(apply_leg(kind, elsewhere), want, "{}", kind.as_str());
        }
    }

    /// `last_error` and `not_published` never arrive together. The benign
    /// 404 must not survive a channel that has already served a manifest.
    #[test]
    fn empty_channel_is_benign_only_until_a_manifest_has_been_seen() {
        let (err, not_published) = classify_failure(&FeedError::NotPublished, false);
        assert_eq!(err, None);
        assert!(not_published);

        let (err, not_published) = classify_failure(&FeedError::NotPublished, true);
        assert_eq!(
            err.as_deref(),
            Some("no release has been published on this channel yet")
        );
        assert!(!not_published);

        for had_manifest in [false, true] {
            let (err, not_published) = classify_failure(
                &FeedError::Failed("feed returned HTTP 500".into()),
                had_manifest,
            );
            assert_eq!(err.as_deref(), Some("feed returned HTTP 500"));
            assert!(!not_published);
        }
    }

    #[test]
    fn stale_math() {
        let mk = |serial| Snapshot {
            checked: Some(Checked {
                manifest: manifest::parse_verified(
                    serde_json::to_vec(&serde_json::json!({
                        "schema": 1, "channel": "stable", "serial": serial,
                        "version": "0.23.0",
                        "notes_url": "https://git.unom.io/unom/punktfunk/releases",
                    }))
                    .unwrap()
                    .as_slice(),
                    "stable",
                )
                .unwrap(),
                fetched_unix: crate::clock::unix_secs_u64(),
            }),
            last_error: None,
            not_published: false,
            job: None,
            last_result: None,
            source_behind: None,
        };
        assert!(!mk(crate::clock::unix_secs_u64()).stale());
        assert!(mk(crate::clock::unix_secs_u64() - STALE_AFTER.as_secs() - 10).stale());
    }

    #[test]
    fn a_source_build_is_current_only_when_git_says_so() {
        // An unanswered fetch is not "up to date": the Deck keeps its last count and the
        // console shows no update rather than promising one it cannot check.
        assert!(source_newer(Some(3)));
        assert!(!source_newer(Some(0)));
        assert!(!source_newer(None));
    }

    /// A canary Deck stamps the canary base, so the feed's run-number compare cannot answer
    /// for it. Apply must still accept what status offered.
    #[test]
    fn a_deck_build_is_offered_what_its_checkout_is_behind() {
        use detect::{Channel, InstallKind};
        let m: Manifest = serde_json::from_value(serde_json::json!({
            "schema": 1, "channel": "canary", "serial": 1,
            "version": "0.41.0~ci32773.gdeadbeef", "ci_run": 32773,
        }))
        .unwrap();
        let deck = "0.41.0+g17d166764";
        let offer = |kind, behind| offers_update(kind, Channel::Canary, deck, &m, behind);
        assert!(offer(InstallKind::SteamosSource, Some(2)));
        assert!(!offer(InstallKind::SteamosSource, Some(0)));
        assert!(!offer(InstallKind::Pacman, Some(2)));
    }
}
