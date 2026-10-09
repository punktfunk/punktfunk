//! Apply-job bookkeeping: the snapshot the console polls, and two on-disk records
//! so an update can report its outcome across its own restart.
//!
//! - Intent (`update-intent.json`): written just before anything irreversible.
//!   It is the only witness once the installer kills this process.
//! - Result (`update-result.json`): durable outcome of the last apply, written
//!   by a failing stage or by boot-time reconciliation.
//!
//! [`reconcile`] once per boot: intent + running the target version ⇒ success;
//! intent + still the old version after the grace window ⇒ failure with the
//! installer log; a fresh intent ⇒ still in flight. Pure over its inputs.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Seconds after intent-write that an unchanged running version is still "in flight".
/// Past this, a restart without the new version is a failed apply — never silent.
pub(crate) const APPLY_GRACE_SECS: u64 = 10 * 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct IntentRecord {
    pub from: String,
    pub to: String,
    pub serial: u64,
    pub started_unix: u64,
    /// Ties a result to exact installer bytes.
    pub installer_sha256: String,
    pub log_path: String,
    /// Source rebuild (Steam Deck `update.sh`). Version equality proves nothing —
    /// the workspace version only moves on bumps. Presence of this intent at boot
    /// is the success signal: the script restarts the host only after install.
    #[serde(default)]
    pub source_build: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ResultRecord {
    pub ok: bool,
    pub from: String,
    pub to: String,
    pub finished_unix: u64,
    /// Failed stage: `downloading` | `verifying` | `applying` | `restarting`. Absent on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,
    /// Applied, but activates on the next reboot (rpm-ostree).
    #[serde(default)]
    pub staged: bool,
}

/// Live job the console polls (`GET /update/status`).
#[derive(Debug, Clone)]
pub(crate) struct JobSnapshot {
    pub target_version: String,
    /// `downloading` | `verifying` | `applying` | `restarting`.
    pub stage: &'static str,
    pub received_bytes: u64,
    pub total_bytes: Option<u64>,
    pub started_unix: u64,
}

pub(crate) fn intent_path() -> PathBuf {
    pf_paths::config_dir().join("update-intent.json")
}

pub(crate) fn result_path() -> PathBuf {
    pf_paths::config_dir().join("update-result.json")
}

pub(crate) fn read_intent(path: &Path) -> Option<IntentRecord> {
    let bytes = std::fs::read(path).ok()?;
    // Unparseable intent is "no intent". Reconcile must not crash on a disk fault.
    serde_json::from_slice(&bytes).ok()
}

pub(crate) fn read_result(path: &Path) -> Option<ResultRecord> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// [`pf_paths::replace_file`]. Intent and result records must never be half-written.
pub(crate) fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    pf_paths::replace_file(path, &bytes)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Reconciled {
    None,
    /// Fresh intent, still the old version: installer may not have stopped us yet.
    StillApplying,
    Success(ResultRecord),
    /// Wrong version after the grace window.
    Failed(ResultRecord),
}

/// No I/O: current version and wall clock versus the intent.
pub(crate) fn reconcile(
    intent: Option<IntentRecord>,
    current_version: &str,
    now_unix: u64,
) -> Reconciled {
    let Some(intent) = intent else {
        return Reconciled::None;
    };
    if intent.source_build {
        // Presence at boot is success. `to` is the running version; a rebuild may keep it.
        return Reconciled::Success(ResultRecord {
            ok: true,
            from: intent.from,
            to: current_version.to_string(),
            finished_unix: now_unix,
            stage: None,
            error: None,
            log_path: Some(intent.log_path),
            staged: false,
        });
    }
    if current_version == intent.to {
        return Reconciled::Success(ResultRecord {
            ok: true,
            from: intent.from,
            to: intent.to,
            finished_unix: now_unix,
            stage: None,
            error: None,
            log_path: Some(intent.log_path),
            staged: false,
        });
    }
    if now_unix.saturating_sub(intent.started_unix) < APPLY_GRACE_SECS {
        return Reconciled::StillApplying;
    }
    Reconciled::Failed(ResultRecord {
        ok: false,
        from: intent.from.clone(),
        to: intent.to.clone(),
        finished_unix: now_unix,
        stage: Some("restarting".into()),
        error: Some(format!(
            "the host restarted still running {} (expected {}) — the installer aborted or \
             rolled back; see its log",
            intent.from, intent.to
        )),
        log_path: Some(intent.log_path),
        staged: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(started: u64) -> IntentRecord {
        IntentRecord {
            from: "0.23.100".into(),
            to: "0.23.200".into(),
            serial: 42,
            started_unix: started,
            installer_sha256: "ab".repeat(32),
            log_path: "/logs/update-0.23.200.log".into(),
            source_build: false,
        }
    }

    #[test]
    fn an_intent_without_the_tray_field_deserializes_as_false() {
        let json = r#"{"from":"0.23.100","to":"0.23.200","serial":42,"started_unix":1000,
            "installer_sha256":"ab","log_path":"/logs/x.log"}"#;
        let i: IntentRecord = serde_json::from_str(json).expect("older intent still parses");
        assert!(!i.source_build);
    }

    #[test]
    fn source_build_intent_is_success_with_the_running_version() {
        let mut i = intent(0);
        i.source_build = true;
        // Rebuild without a bump: still success, and `to` is what runs, not the manifest label.
        match reconcile(Some(i), "0.23.100", 10_000_000) {
            Reconciled::Success(r) => {
                assert!(r.ok);
                assert_eq!(r.to, "0.23.100");
            }
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[test]
    fn no_intent_is_none() {
        assert_eq!(reconcile(None, "0.23.100", 1000), Reconciled::None);
    }

    #[test]
    fn target_version_is_success_regardless_of_age() {
        for now in [1, 10_000_000] {
            match reconcile(Some(intent(0)), "0.23.200", now) {
                Reconciled::Success(r) => {
                    assert!(r.ok);
                    assert_eq!(r.to, "0.23.200");
                    assert_eq!(r.log_path.as_deref(), Some("/logs/update-0.23.200.log"));
                }
                other => panic!("expected success, got {other:?}"),
            }
        }
    }

    #[test]
    fn fresh_intent_old_version_is_still_applying() {
        assert_eq!(
            reconcile(Some(intent(1000)), "0.23.100", 1000 + APPLY_GRACE_SECS - 1),
            Reconciled::StillApplying
        );
    }

    #[test]
    fn stale_intent_old_version_is_failure_with_log() {
        match reconcile(Some(intent(1000)), "0.23.100", 1000 + APPLY_GRACE_SECS) {
            Reconciled::Failed(r) => {
                assert!(!r.ok);
                assert_eq!(r.stage.as_deref(), Some("restarting"));
                assert!(r.error.as_deref().unwrap().contains("0.23.200"));
                assert!(r.log_path.is_some());
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[test]
    fn intent_and_result_roundtrip_atomically() {
        let dir = std::env::temp_dir().join(format!("pf-update-jobs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ip = dir.join("update-intent.json");
        write_json_atomic(&ip, &intent(7)).unwrap();
        assert_eq!(read_intent(&ip).unwrap().started_unix, 7);
        std::fs::write(&ip, b"{half").unwrap();
        assert!(read_intent(&ip).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
