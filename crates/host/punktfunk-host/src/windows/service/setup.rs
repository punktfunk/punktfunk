//! `service install` / `uninstall` / `restart` and the `sc.exe` pass-through.

use super::*;

pub(super) fn install(args: &[String]) -> Result<()> {
    use windows_service::service::{
        ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl,
        ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo, ServiceStartType,
        ServiceType,
    };
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    // `None` = flag absent: the store keeps its value.
    let gamestream = match args.iter().find_map(|a| a.strip_prefix("--gamestream=")) {
        Some("on") => Some(true),
        Some("off") => Some(false),
        Some(v) => bail!("--gamestream must be 'on' or 'off' (got '{v}')"),
        None => None,
    };
    // `None` = flag absent: host.env keeps its bind.
    let mgmt_bind = match args.iter().find_map(|a| a.strip_prefix("--mgmt-bind=")) {
        Some(v) => match v.parse::<std::net::SocketAddr>() {
            Ok(addr) => Some(addr),
            Err(_) => bail!("--mgmt-bind must be IP:PORT (got '{v}')"),
        },
        None => None,
    };
    // Where the web console listens. Address only — the port is the console's own.
    let web_bind = match args.iter().find_map(|a| a.strip_prefix("--web-bind=")) {
        Some(v) => match v.parse::<std::net::IpAddr>() {
            Ok(ip) => Some(ip),
            Err(_) => bail!("--web-bind must be an IP address (got '{v}')"),
        },
        None => None,
    };

    let exe = std::env::current_exe().context("current_exe")?;
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("open Service Control Manager (run from an elevated/Administrator prompt)")?;

    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.clone(),
        launch_arguments: vec![OsString::from("service"), OsString::from("run")],
        dependencies: vec![],
        account_name: None, // None = LocalSystem
        account_password: None,
    };

    // Idempotent: create, or reconfigure if it already exists.
    match manager.create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START) {
        Ok(svc) => {
            let _ = svc.set_description(SERVICE_DESCRIPTION);
            println!("Created service '{SERVICE_NAME}' (auto-start, LocalSystem).");
        }
        Err(windows_service::Error::Winapi(e))
            if e.raw_os_error() == Some(1073 /* ERROR_SERVICE_EXISTS */) =>
        {
            let svc = manager
                .open_service(SERVICE_NAME, ServiceAccess::CHANGE_CONFIG)
                .context("open existing service to reconfigure")?;
            svc.change_config(&info)
                .context("reconfigure existing service")?;
            let _ = svc.set_description(SERVICE_DESCRIPTION);
            println!("Reconfigured existing service '{SERVICE_NAME}'.");
        }
        Err(e) => return Err(e).context("create service"),
    }

    // Restart 1 s / 5 s / 60 s (SCM repeats the last action). Resets after a clean day. Fires
    // only on a crash, never a deliberate stop. Best-effort. Fresh open: restart needs SERVICE_START.
    let recovery = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::CHANGE_CONFIG | ServiceAccess::START,
        )
        .and_then(|svc| {
            svc.update_failure_actions(ServiceFailureActions {
                reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 60 * 60)),
                reboot_msg: None,
                command: None,
                actions: Some(vec![
                    ServiceAction {
                        action_type: ServiceActionType::Restart,
                        delay: Duration::from_secs(1),
                    },
                    ServiceAction {
                        action_type: ServiceActionType::Restart,
                        delay: Duration::from_secs(5),
                    },
                    ServiceAction {
                        action_type: ServiceActionType::Restart,
                        delay: Duration::from_secs(60),
                    },
                ]),
            })
        });
    match recovery {
        Ok(()) => println!("Crash recovery: the SCM restarts the service at 1s/5s/60s."),
        Err(e) => eprintln!("warning: service recovery actions not set: {e}"),
    }

    ensure_default_host_env()?;
    apply_gamestream_choice(gamestream);
    // Before the rules below: the mgmt rule reads its port back from host.env.
    if let Some(addr) = mgmt_bind {
        set_host_env_line("PUNKTFUNK_MGMT_BIND", &addr.to_string())?;
    }
    if let Some(ip) = web_bind {
        set_host_env_line("PUNKTFUNK_UI_BIND", &ip.to_string())?;
    }
    // Remove prior rules first so an upgrade tightens scope instead of leaving a stale
    // all-profiles rule. Flag absent (upgrades) keeps the recorded choice.
    let allow_public = allow_public_network(args)?;
    set_fw_public_marker(allow_public);
    remove_firewall_rules();
    add_firewall_rules(allow_public);

    println!(
        "\nInstalled. Config: {}\nLogs:   {}\n\nStart now with:  punktfunk-host service start",
        host_env_path().display(),
        pf_paths::config_dir().join("logs").display()
    );
    Ok(())
}

/// Stop, wait until Stopped, then start. A bare `sc stop && sc start` races: START fails with
/// "instance already running" while the old process winds down.
pub(super) fn restart() -> Result<()> {
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("open Service Control Manager (run elevated)")?;
    let svc = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::STOP | ServiceAccess::QUERY_STATUS | ServiceAccess::START,
        )
        .context("open service (run elevated)")?;
    // ERROR_SERVICE_NOT_ACTIVE means restart == start.
    let _ = svc.stop();
    wait_stopped(&svc)?;
    svc.start(&[] as &[&std::ffi::OsStr])
        .context("start service")?;
    println!("Restarted service '{SERVICE_NAME}'.");
    Ok(())
}

/// Poll until the SCM reports the service stopped, 30 s at most. A stop is asynchronous: a
/// delete or a start issued before this lands races the still-running host.
pub(super) fn wait_stopped(svc: &windows_service::service::Service) -> Result<()> {
    use windows_service::service::ServiceState;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let state = svc.query_status().context("query service status")?;
        if state.current_state == ServiceState::Stopped {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("service did not stop within 30 s");
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

pub(super) fn uninstall() -> Result<()> {
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("open Service Control Manager (run elevated)")?;
    let svc = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::STOP | ServiceAccess::QUERY_STATUS | ServiceAccess::DELETE,
        )
        .context("open service for delete")?;
    // ERROR_SERVICE_NOT_ACTIVE means there is nothing to wait for.
    let _ = svc.stop();
    wait_stopped(&svc)?;
    svc.delete().context("delete service")?;
    remove_firewall_rules();
    println!("Removed service '{SERVICE_NAME}' and its firewall rules.");
    Ok(())
}

/// `sc.exe` with output passed through (start/stop/status).
///
/// Absolute: `CreateProcess` searches the cwd before `%PATH%`, and this runs elevated.
pub(super) fn sc(args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(crate::install::resolve_tool("sc"))
        .args(args)
        .status()
        .context("run sc.exe")?;
    if !status.success() {
        bail!("sc {} failed ({status})", args.join(" "));
    }
    Ok(())
}
