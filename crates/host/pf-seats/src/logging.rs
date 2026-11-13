//! Where the supervisor's own diagnostics go: stderr. Nothing here may record a credential:
//! seat passwords, the DPAPI blob and the control bearer never reach a `tracing` field.

/// Operator commands report through stderr so stdout stays the machine-readable
/// channel `--json` promises.
pub fn to_stderr() {
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .try_init();
}
