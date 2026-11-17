//! Where the supervisor's own diagnostics go: stderr. Nothing here may record a credential:
//! seat passwords and the DPAPI blob never reach a `tracing` field.

/// Logs to stderr so stdout stays free for a command's own output, such as the code
/// `pam-check` prints.
pub fn to_stderr() {
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .try_init();
}
