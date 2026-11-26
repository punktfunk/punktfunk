//! `punktfunk-seat-keeper`: holds one seat's RDP session open. Its verbs:
//!
//! - `keep` holds the session. It reads its bootstrap (account, password, pin, host root) from
//!   stdin, which only the supervisor writes. It never takes a secret on argv or from the
//!   environment.
//! - `trust [--replace]` records the pin. It is the elevated operator step that pins the TLS
//!   leaf certificate of TermService before any seat starts. It refuses a leaf the machine
//!   doesn't hold in its Remote Desktop certificate stores.
//! - `observe` prints the leaf certificate TermService presents now, and stores nothing.

#[cfg(windows)]
mod rdp;

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let outcome = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["keep"] => rdp::keep_from_stdin(),
        ["trust"] => trust(false),
        ["trust", "--replace"] => trust(true),
        ["observe"] => observe(),
        _ => {
            eprintln!("usage: punktfunk-seat-keeper keep | trust [--replace] | observe");
            std::process::exit(2);
        }
    };
    if let Err(error) = outcome {
        eprintln!("punktfunk-seat-keeper: {error}");
        std::process::exit(1);
    }
}

/// Records the leaf TermService presents when the machine holds it (`keeper::machine_pins`).
/// Without `replace` a different stored pin stays and the command fails.
#[cfg(windows)]
fn trust(replace: bool) -> Result<(), pf_seats::BackendError> {
    use pf_seats::windows::keeper;
    keeper::require_trust_prerequisites()?;
    let root = pf_seats::SecretRoot::open(pf_seats::persistence::default_root())
        .map_err(|error| rdp::io_error("seat_root", "open hardened seat root", error))?;
    let pin = rdp::observe_pin()?;
    pf_seats::pin::accept_leaf(pin, &keeper::machine_pins())
        .map_err(|cause| rdp::io_error("rdp_leaf_not_held", "trust RDP leaf", cause))?;
    keeper::store_pin(&root, pin, replace)?;
    println!("trusted RDP leaf SHA-256 {}", hex::encode(pin));
    Ok(())
}

#[cfg(windows)]
fn observe() -> Result<(), pf_seats::BackendError> {
    println!("{}", hex::encode(rdp::observe_pin()?));
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("punktfunk-seat-keeper runs on Windows only");
    std::process::exit(2);
}
