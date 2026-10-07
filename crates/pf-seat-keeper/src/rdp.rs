//! Pinned IronRDP keeper and credential-free trust handshake.
//!
//! Both roles connect only to `127.0.0.2:3389`, advertise CredSSP/NLA without
//! legacy TLS login, and require a HYBRID security selection. `trust` stops
//! immediately after RDP negotiation and the TLS handshake, before CredSSP, and
//! reports the observed SHA-256 leaf pin. Runtime TLS verifies that exact pin
//! before `connect_finalize` can serialize credentials. The keeper requests a
//! 640x480 desktop, processes protocol responses, discards decoded pixels, and
//! remains connected until its job is closed or the server disconnects it.

use ironrdp::connector::{
    self, ClientConnector, ClientConnectorState, ConnectionResult, Credentials,
};
use ironrdp::graphics::image_processing::PixelFormat;
use ironrdp::pdu::gcc::KeyboardType;
use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp::pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStage, ActiveStageOutput};
use pf_seats::windows::keeper::{self, RdpBootstrap};
use pf_seats::BackendError;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use x509_cert::der::Decode as _;
use zeroize::Zeroize as _;

type WinResult<T> = Result<T, BackendError>;

const RDP_ADDRESS: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)), 3389);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const ACTIVE_READ_TIMEOUT: Duration = Duration::from_secs(2);

type TlsStream = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;
type RdpFramed = ironrdp_blocking::Framed<TlsStream>;
/// Connector after negotiation, the TLS-wrapped framed stream, the leaf public
/// key CredSSP binds to, and the observed leaf pin.
type Negotiated = (ClientConnector, RdpFramed, Vec<u8>, Option<[u8; 32]>);

fn backend_error(code: &str, message: impl Into<String>) -> BackendError {
    BackendError::new(code, message)
}

pub(crate) fn io_error(code: &str, context: &str, error: impl std::fmt::Display) -> BackendError {
    backend_error(code, format!("{context}: {error}"))
}

/// The SHA-256 of TermService's TLS leaf, read before any credential could be sent.
pub(crate) fn observe_pin() -> WinResult<[u8; 32]> {
    let config = connector_config(String::new(), String::new(), None);
    let (_connector, _framed, _public_key, observed) = negotiate_tls(config, None)?;
    observed.ok_or_else(|| {
        backend_error(
            "rdp_certificate",
            "RDP TLS handshake did not expose a leaf certificate",
        )
    })
}

/// Reattach attempts before the keeper gives the seat back to the supervisor. Ten at the delay
/// below covers a graphics-subsystem reset without letting a seat that cannot hold a session spin.
const REATTACH_ATTEMPTS: u32 = 10;
/// Long enough for the server's graphics subsystem to finish resetting before the next connect.
const REATTACH_DELAY: Duration = Duration::from_secs(3);

/// Hold the seat's session open, reattaching whenever the connection drops but the session does
/// not. A neighbouring seat logging off knocks this connection over — the server reports its
/// graphics subsystem in an error state — and the session survives as `Disconnected`, so exiting
/// there would strand a seat that only needed reconnecting.
///
/// The session's existence is the test, not the disconnect reason: a logoff removes it, anything
/// recoverable leaves it. That needs no match on the server's English description.
pub(crate) fn keep_from_stdin() -> WinResult<()> {
    let mut input = std::io::stdin().lock();
    let mut bootstrap = RdpBootstrap::read_from(&mut input)
        .map_err(|error| io_error("keeper_bootstrap", "read RDP keeper bootstrap", error))?;
    let domain = Some(keeper::computer_name()?);
    let account = bootstrap.account.clone();
    let mut outcome = keep_session(&bootstrap, domain.clone(), &account);
    for attempt in 1..=REATTACH_ATTEMPTS {
        let Err(error) = outcome else { return Ok(()) };
        // No session left means the seat is gone (logged off, or it never came up): hand the
        // error back so the supervisor restarts the seat rather than reconnecting to nothing.
        match keeper::session_of(&account) {
            Ok(Some(session)) => eprintln!(
                "seat keeper: reattaching to session {session} after {error} (attempt {attempt}/{REATTACH_ATTEMPTS})"
            ),
            _ => {
                bootstrap.password.zeroize();
                return Err(error);
            }
        }
        std::thread::sleep(REATTACH_DELAY);
        outcome = keep_session(&bootstrap, domain.clone(), &account);
    }
    bootstrap.password.zeroize();
    outcome
}

/// One connect-and-hold. The password is rebuilt per attempt because the connector consumes it.
fn keep_session(bootstrap: &RdpBootstrap, domain: Option<String>, account: &str) -> WinResult<()> {
    let config = connector_config(account.to_owned(), bootstrap.password.to_string(), domain);
    let (mut connector, mut framed, public_key, observed) =
        negotiate_tls(config, Some(bootstrap.pin))?;
    if observed != Some(bootstrap.pin) {
        return Err(backend_error(
            "rdp_pin_changed",
            "RDP leaf certificate does not match the trusted pin",
        ));
    }
    let upgraded = ironrdp_blocking::mark_as_upgraded(
        ironrdp_blocking::skip_connect_begin(&mut connector),
        &mut connector,
    );
    let mut network = NoExternalNetwork;
    let result = ironrdp_blocking::connect_finalize(
        upgraded,
        connector,
        &mut framed,
        &mut network,
        "localhost".to_owned().into(),
        public_key,
        None,
    )
    .map_err(|error| {
        io_error(
            "rdp_connect",
            "IronRDP connection finalization failed",
            error,
        )
    })?;
    active_loop(result, framed)
}

fn connector_config(
    username: String,
    password: String,
    domain: Option<String>,
) -> connector::Config {
    connector::Config {
        credentials: Credentials::UsernamePassword { username, password },
        domain,
        enable_tls: false,
        enable_credssp: true,
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_layout: 0,
        keyboard_functional_keys_count: 12,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        desktop_size: connector::DesktopSize {
            width: 640,
            height: 480,
        },
        bitmap: None,
        client_build: 0,
        client_name: "PunktfunkSeats".to_owned(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        platform: MajorPlatformType::WINDOWS,
        enable_server_pointer: false,
        request_data: None,
        autologon: false,
        // A remote session has NO audio endpoint of its own: with playback redirection off,
        // MMDevice enumerates zero renders inside it, so a seat host has nothing to
        // loopback-capture and desktop audio can never open. Negotiating it gives the session its
        // own render endpoint, which is also what keeps one seat's audio out of another's.
        enable_audio_playback: true,
        pointer_software_rendering: false,
        performance_flags: PerformanceFlags::default(),
        desktop_scale_factor: 100,
        hardware_id: None,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        compression_type: None,
        multitransport_flags: None,
    }
}

fn negotiate_tls(
    config: connector::Config,
    expected_pin: Option<[u8; 32]>,
) -> WinResult<Negotiated> {
    let stream = TcpStream::connect_timeout(&RDP_ADDRESS, CONNECT_TIMEOUT)
        .map_err(|error| io_error("rdp_connect", "connect to 127.0.0.2:3389", error))?;
    stream
        .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(|error| io_error("rdp_connect", "set RDP read timeout", error))?;
    stream
        .set_write_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(|error| io_error("rdp_connect", "set RDP write timeout", error))?;
    let client_addr = stream
        .local_addr()
        .map_err(|error| io_error("rdp_connect", "read RDP client address", error))?;
    let mut framed = ironrdp_blocking::Framed::new(stream);
    let mut connector = ClientConnector::new(config, client_addr);
    ironrdp_blocking::connect_begin(&mut framed, &mut connector)
        .map_err(|error| io_error("rdp_negotiation", "RDP negotiation failed", error))?;
    let selected = match connector.state {
        ClientConnectorState::EnhancedSecurityUpgrade { selected_protocol } => selected_protocol,
        _ => {
            return Err(backend_error(
                "rdp_negotiation",
                "IronRDP did not stop at the TLS security upgrade",
            ));
        }
    };
    if !selected.intersects(
        ironrdp::pdu::nego::SecurityProtocol::HYBRID
            | ironrdp::pdu::nego::SecurityProtocol::HYBRID_EX,
    ) {
        return Err(backend_error(
            "rdp_nla_required",
            "RDP server did not select CredSSP/NLA",
        ));
    }
    let stream = framed.into_inner_no_leftover();
    let observed = Arc::new(Mutex::new(None));
    let verifier = pf_seats::pin::PinVerify::with_observed(expected_pin, observed.clone());
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut tls_config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| io_error("rdp_tls", "select TLS protocol versions", error))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    tls_config.resumption = rustls::client::Resumption::disabled();
    let server_name = rustls::pki_types::ServerName::try_from("localhost")
        .map_err(|error| io_error("rdp_tls", "build TLS server name", error))?;
    let mut connection = rustls::ClientConnection::new(Arc::new(tls_config), server_name)
        .map_err(|error| io_error("rdp_tls", "create TLS connection", error))?;
    let mut stream = stream;
    while connection.is_handshaking() {
        connection
            .complete_io(&mut stream)
            .map_err(|error| io_error("rdp_tls", "RDP TLS handshake failed", error))?;
    }
    let certificate = connection
        .peer_certificates()
        .and_then(|certificates| certificates.first())
        .ok_or_else(|| backend_error("rdp_certificate", "RDP server sent no leaf certificate"))?;
    let public_key = extract_public_key(certificate.as_ref())?;
    let observed = *observed.lock().unwrap_or_else(|poison| poison.into_inner());
    stream
        .set_read_timeout(Some(ACTIVE_READ_TIMEOUT))
        .map_err(|error| io_error("rdp_connect", "set active RDP timeout", error))?;
    let tls = rustls::StreamOwned::new(connection, stream);
    Ok((
        connector,
        ironrdp_blocking::Framed::new(tls),
        public_key,
        observed,
    ))
}

fn extract_public_key(certificate: &[u8]) -> WinResult<Vec<u8>> {
    let certificate = x509_cert::Certificate::from_der(certificate)
        .map_err(|error| io_error("rdp_certificate", "parse RDP leaf certificate", error))?;
    certificate
        .tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .as_bytes()
        .map(ToOwned::to_owned)
        .ok_or_else(|| backend_error("rdp_certificate", "RDP leaf public key is not byte-aligned"))
}

fn active_loop(result: ConnectionResult, mut framed: RdpFramed) -> WinResult<()> {
    let mut image = DecodedImage::new(
        PixelFormat::RgbA32,
        result.desktop_size.width,
        result.desktop_size.height,
    );
    let mut stage = ActiveStage::new(result);
    loop {
        let (action, payload) = match framed.read_pdu() {
            Ok(frame) => frame,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(error) => {
                return Err(io_error("rdp_disconnected", "read active RDP PDU", error));
            }
        };
        let outputs = stage
            .process(&mut image, action, &payload)
            .map_err(|error| io_error("rdp_protocol", "process active RDP PDU", error))?;
        for output in outputs {
            match output {
                ActiveStageOutput::ResponseFrame(frame) => {
                    framed.write_all(&frame).map_err(|error| {
                        io_error("rdp_protocol", "write active RDP response", error)
                    })?
                }
                ActiveStageOutput::Terminate(reason) => {
                    return Err(backend_error(
                        "rdp_disconnected",
                        format!("RDP server ended the managed session: {reason:?}"),
                    ));
                }
                ActiveStageOutput::DeactivateAll(_) => {
                    return Err(backend_error(
                        "rdp_deactivated",
                        "RDP server deactivated the managed session",
                    ));
                }
                // No sideband UDP transport and no use for the server's RTT
                // figures: the keeper only has to hold the session open.
                ActiveStageOutput::GraphicsUpdate(_)
                | ActiveStageOutput::PointerDefault
                | ActiveStageOutput::PointerHidden
                | ActiveStageOutput::PointerPosition { .. }
                | ActiveStageOutput::PointerBitmap(_)
                | ActiveStageOutput::MultitransportRequest(_)
                | ActiveStageOutput::AutoDetect(_) => {}
            }
        }
    }
}

struct NoExternalNetwork;

impl connector::sspi::network_client::NetworkClient for NoExternalNetwork {
    fn send(
        &self,
        _request: &connector::sspi::generator::NetworkRequest,
    ) -> connector::sspi::Result<Vec<u8>> {
        Err(connector::sspi::Error::new(
            connector::sspi::ErrorKind::NoAuthenticatingAuthority,
            "external Kerberos transport is disabled for local seat login",
        ))
    }
}
