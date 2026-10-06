//! Per-user system-tray companion for the punktfunk host.
//!
//! Icon and menu: running / stopped / degraded / failed, plus open-console,
//! start/stop/restart (UAC per action on Windows; on Linux `systemctl --user` per
//! unit for the host, web console and plugin runner), pairing, and exit.
//!
//! Process state is SCM / the systemd user unit first; a listener on the mgmt
//! port cannot make a stopped service look running. Streaming detail is
//! loopback `GET /api/v1/local/summary` on `<config_dir>/tray-token`. `--mgmt-port`
//! pins the port; otherwise it follows `<config_dir>/mgmt-endpoint`, then 47990.
//!
//! Poller: `status.rs`. Linux SNI: `linux.rs`. Windows notify-icon: `win.rs`.
//!
//! `#![windows_subsystem = "windows"]`: a console-subsystem exe in the HKLM
//! Run key flashes a terminal at every sign-in.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(target_os = "linux")]
mod linux;
#[cfg(any(windows, target_os = "linux"))]
mod status;
#[cfg(windows)]
mod win;
#[cfg(windows)]
mod win_theme;

/// The tray cannot read `host.env` on Windows (DACL-locked to
/// SYSTEM/Administrators); `--mgmt-port` still pins the port.
pub struct Args {
    /// Windows uninstaller: ask this session's instance to exit.
    pub quit: bool,
    /// Autostart: exit silently when this user is not a host (Linux installs it for every user).
    pub autostart: bool,
    /// Start-menu launcher: start a stopped host once — behind one UAC prompt on
    /// Windows, with its web console and plugin runner on Linux.
    pub start_host: bool,
    /// Loopback by default; the summary route rejects anything else.
    pub mgmt_addr: String,
    /// `None` re-reads the published endpoint every poll.
    pub mgmt_port: Option<u16>,
    pub web_port: u16,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            quit: false,
            autostart: false,
            start_host: false,
            mgmt_addr: "127.0.0.1".into(),
            mgmt_port: None,
            web_port: 47992,
        }
    }
}

fn parse_args() -> anyhow::Result<Args> {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut value = |flag: &str| {
            it.next()
                .ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))
        };
        match a.as_str() {
            "--quit" => args.quit = true,
            "--autostart" => args.autostart = true,
            "--start-host" => args.start_host = true,
            "--mgmt-addr" => args.mgmt_addr = value("--mgmt-addr")?,
            "--mgmt-port" => args.mgmt_port = Some(value("--mgmt-port")?.parse()?),
            "--web-port" => args.web_port = value("--web-port")?.parse()?,
            "--version" | "-V" => {
                println!("punktfunk-tray {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other => anyhow::bail!(
                "unknown argument '{other}'\n\nUSAGE:\n    punktfunk-tray [--autostart] [--quit] \
                 [--start-host] [--mgmt-addr <IP>] [--mgmt-port <N>] [--web-port <N>]"
            ),
        }
    }
    Ok(args)
}

fn main() -> anyhow::Result<()> {
    // Same cfg as `run`; other targets stub and never poll TLS.
    #[cfg(any(windows, target_os = "linux"))]
    punktfunk_core::tls::install_default_provider();
    let args = parse_args()?;
    run(args)
}

#[cfg(windows)]
fn run(args: Args) -> anyhow::Result<()> {
    win::run(args)
}

#[cfg(target_os = "linux")]
fn run(args: Args) -> anyhow::Result<()> {
    linux::run(args)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn run(_args: Args) -> anyhow::Result<()> {
    anyhow::bail!("punktfunk-tray supports Windows and Linux hosts only")
}
