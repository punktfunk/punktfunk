//! Windows helper spawned by `punktfunk-host` for one mirror or shared-screen session. The
//! host hands it two pipe ends (`--control`); the run loop is [`punktfunk_capture_worker::run`].
//!
//! It runs as the signed-in user. Never start it as SYSTEM: the capture does not activate.

fn main() -> std::process::ExitCode {
    // Stderr is a pipe the host relays into its own log, so no colour and no timestamps of ours.
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .without_time()
        .init();

    #[cfg(target_os = "windows")]
    {
        let args: Vec<String> = std::env::args().skip(1).collect();
        match punktfunk_capture_worker::run(&args) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "punktfunk-capture-worker exiting");
                std::process::ExitCode::FAILURE
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        tracing::error!(
            "punktfunk-capture-worker is a Windows-only helper and has nothing to do here"
        );
        std::process::ExitCode::FAILURE
    }
}
