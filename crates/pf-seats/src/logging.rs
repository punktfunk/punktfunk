//! Where the supervisor's own diagnostics go.
//!
//! A Windows service has no console, so the SCM path writes to `seats.log`
//! beside the ledger and every operator command writes to stderr. Nothing here
//! may record a credential: seat passwords, the DPAPI blob and the control
//! bearer never reach a `tracing` field.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

pub const LOG_FILE: &str = "seats.log";

/// Sends the supervisor's records to `<root>/seats.log`, appended across
/// restarts so a failure that killed the service is still readable.
///
/// The file is reopened per record rather than held: this log carries a
/// handful of lines per command, and a held handle would keep the ledger
/// directory busy for the whole service lifetime. A record that cannot be
/// written falls back to stderr instead of being lost.
pub fn to_file(root: &Path) {
    let path = root.join(LOG_FILE);
    if OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .is_err()
    {
        to_stderr();
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || -> Box<dyn Write> {
            match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(file) => Box::new(file),
                Err(_) => Box::new(std::io::stderr()),
            }
        })
        .try_init();
}

/// Operator commands report through stderr so stdout stays the machine-readable
/// channel `--json` promises.
pub fn to_stderr() {
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .try_init();
}
