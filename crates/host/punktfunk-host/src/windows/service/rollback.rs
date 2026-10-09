//! Boot-loop rollback after a host update.

/// Boot-loop rollback after a host update. A fresh intent plus a crash-looping child that *is*
/// the intent's target means the just-installed host does not stay up. Re-run the cached
/// previous installer once per intent, Authenticode-checked. This process is not in the
/// kill-on-close job, so the installer survives the service stop it is about to perform.
/// Evidence: `host-update-from-web-console.md`.
pub(super) fn maybe_boot_loop_rollback(restarts: u32, attempted: &mut bool) {
    if *attempted || restarts < 3 {
        return;
    }
    let intent_path = crate::update::jobs::intent_path();
    let Some(intent) = crate::update::jobs::read_intent(&intent_path) else {
        return;
    };
    let now = crate::clock::unix_secs_u64();
    // Stale intent: no rollback. A boot-looping *old* binary is not this update; reconcile owns it.
    if now.saturating_sub(intent.started_unix) > 30 * 60 || crate::version::get() != intent.to {
        return;
    }
    *attempted = true;

    let updates = pf_paths::config_dir().join("updates");
    let previous = std::fs::read_dir(&updates)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| {
                    n.starts_with("punktfunk-host-setup-")
                        && n.ends_with(".exe")
                        && !n.contains(intent.to.as_str())
                })
                .unwrap_or(false)
        })
        .max_by_key(|p| {
            p.metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        });
    let Some(previous) = previous else {
        tracing::error!(
            to = %intent.to,
            "updated host is crash-looping and no cached previous installer exists — \
             leaving the intent for reconcile; manual reinstall required"
        );
        return;
    };
    // The re-check is signature only, so the directory must still be admin-only: a planted
    // `updates\` would otherwise make a self-signed exe the rollback target.
    if let Some(dir) = previous.parent() {
        if let Err(e) = crate::install::ensure_admin_only_source(dir) {
            tracing::error!(dir = %dir.display(), error = %format!("{e:#}"), "not rolling back");
            return;
        }
    }
    if let Err(e) = crate::update::windows::verify_authenticode(&previous, &[], None) {
        tracing::error!(
            installer = %previous.display(),
            error = %e,
            "cached previous installer fails its signature check — not rolling back"
        );
        return;
    }

    let log = pf_paths::config_dir()
        .join("logs")
        .join(format!("update-rollback-from-{}.log", intent.to));
    let record = crate::update::jobs::ResultRecord {
        ok: false,
        from: intent.from.clone(),
        to: intent.to.clone(),
        finished_unix: now,
        stage: Some("rolled-back".into()),
        error: Some(format!(
            "the updated host crash-looped after install; rolled back via {}",
            previous.display()
        )),
        log_path: Some(log.display().to_string()),
        staged: false,
    };
    let _ = crate::update::jobs::write_json_atomic(&crate::update::jobs::result_path(), &record);
    // Delete the intent first: the incoming host must boot clean, and a missing intent is the one-shot.
    let _ = std::fs::remove_file(&intent_path);

    tracing::warn!(
        failed_version = %intent.to,
        back_to = %previous.display(),
        "updated host is crash-looping — rolling back via the cached previous installer"
    );
    match std::process::Command::new(&previous)
        .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/SP-"])
        .arg(format!("/LOG={}", log.display()))
        .spawn()
    {
        // Detached: it stops this service and reinstalls the previous version.
        Ok(child) => drop(child),
        Err(e) => tracing::error!(error = %e, "rollback installer did not spawn"),
    }
}
