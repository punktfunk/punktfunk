//! Linux helper spawned by `punktfunk-host`: one PyroWave session ([`pf_encode::worker`]), or
//! with `zerocopy-worker` first, one capture's GPU import and fused convert
//! ([`pf_zerocopy::worker`]). Both want the elevated Vulkan queue only this file may hold.
//! The parent `dup2`s the socket onto fd 3 (`--fd` overrides).
//!
//! Only this file may carry `cap_sys_nice=ep`. A hardlink or host subcommand
//! shares the inode and the capability — see this crate's Cargo.toml.

// `forbid`, not `deny`: an `#[allow]` below cannot reopen it.
#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    // Stderr is inherited from the host, so these lines land in the same journal, coloured
    // only on a terminal.
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let fmt = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        fmt.init();
    } else {
        fmt.with_ansi(false).init();
    }

    #[cfg(target_os = "linux")]
    {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let run = match args.first().map(String::as_str) {
            Some("zerocopy-worker") => {
                pf_frame::thread_qos::boost_thread_priority(true);
                pf_zerocopy::worker::run_from_args(&args[1..])
            }
            _ => pf_encode::worker::run_from_args(&args),
        };
        match run {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                // The host already saw the socket die; this line is for `journalctl`.
                tracing::error!(error = %format!("{e:#}"), "punktfunk-encode-worker exiting");
                std::process::ExitCode::FAILURE
            }
        }
    }
    // `VK_KHR_global_priority` + `CAP_SYS_NICE` is Linux. Windows uses WDDM
    // (`D3DKMTSetProcessSchedulingPriorityClass`). Stub keeps the workspace green.
    #[cfg(not(target_os = "linux"))]
    {
        tracing::error!(
            "punktfunk-encode-worker is a Linux-only helper and has nothing to do here"
        );
        std::process::ExitCode::FAILURE
    }
}
