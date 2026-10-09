//! `punktfunk-seat-keeper keep` holds one seat's RDP session open; `trust` records the pin.
//!
//! `keep` reads its bootstrap (account, password, pin) from stdin, which only the supervisor
//! writes, and never takes a secret on argv or from the environment. `trust [--replace]` is the
//! elevated operator step that pins TermService's TLS leaf before any seat starts.

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
        _ => {
            eprintln!("usage: punktfunk-seat-keeper keep | trust [--replace]");
            std::process::exit(2);
        }
    };
    if let Err(error) = outcome {
        eprintln!("punktfunk-seat-keeper: {error}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
fn trust(replace: bool) -> Result<(), pf_seats::BackendError> {
    use pf_seats::windows::keeper;
    keeper::require_trust_prerequisites()?;
    let root = pf_seats::SecretRoot::open(pf_seats::persistence::default_root())
        .map_err(|error| rdp::io_error("seat_root", "open hardened seat root", error))?;
    let pin = rdp::observe_pin()?;
    keeper::store_pin(&root, pin, replace)?;
    println!("trusted RDP leaf SHA-256 {}", hex::encode(pin));
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("punktfunk-seat-keeper runs on Windows only");
    std::process::exit(2);
}
