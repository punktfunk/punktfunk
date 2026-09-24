//! GameStream control: an ENet host on UDP 47999. Moonlight connects it before video
//! (`STAGE_CONTROL_STREAM_START` precedes `STAGE_VIDEO_STREAM_START`); if it is down the
//! whole connection aborts. It carries input, keepalives, and QoS feedback.
//!
//! Sunshine-mode hosts encrypt this stream with AES-128-GCM under the `/launch` `rikey`.
//! Wire (little-endian): `u16 encType=0x0001 | u16 length | u32 seq | [16-byte tag] | ct`,
//! with `length = sizeof(seq) + 16 + plaintext`.
//!
//! Nonce is what Moonlight negotiated (`encryptControlMessage` in moonlight-common-c).
//! `SS_ENC_CONTROL_V2` (stock default): 12-byte nonce, `seq` LE in [0..4], `b"CC"` at
//! [10..12]. Legacy: 16-byte nonce, `iv[0] = seq & 0xff`, rest zero. Tag first, no AAD,
//! key is forward `hex::decode(rikey)`. [`decrypt_control`] locks the scheme on the first
//! authenticating packet.
//!
//! Own native thread, only while a pairing exists. ENet reassembly runs before GCM, so
//! [`sync`] keeps 47999 closed until the first pairing and tears it down when the last
//! one is removed. Pairing itself is HTTPS on nvhttp, never this port.

use super::{AppState, LaunchSession, CONTROL_PORT};
use crate::inject::gamepad::GamepadManager;
use anyhow::{anyhow, Context, Result};
use pf_frame::HdrMeta;
use punktfunk_core::input::{GamepadEvent, InputEvent};
use punktfunk_core::quic::{classify, GrantClass, GRANT_ALL};
use rusty_enet::{Event, Host, HostSettings, Packet, PeerID};
use std::net::{IpAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Binds 47999 only while the paired-client list is non-empty. A never-paired host
/// exposes no ENet at all.
pub(crate) struct Gate {
    /// Armed by `serve` when `--gamestream` is on. Unpair also runs on native-only hosts,
    /// so [`sync`] is a no-op until this is set.
    enabled: AtomicBool,
    /// Live listener; `None` = closed. Held across the bind/teardown decision so a pair
    /// racing an unpair cannot double-bind or leave the port in the wrong state.
    running: Mutex<Option<Running>>,
}

impl Gate {
    pub(crate) fn new() -> Gate {
        Gate {
            enabled: AtomicBool::new(false),
            running: Mutex::new(None),
        }
    }

    /// Arm the gate. [`sync`] is a no-op until this runs from `serve`'s GameStream branch.
    pub(crate) fn enable(&self) {
        self.enabled.store(true, Ordering::SeqCst);
    }
}

struct Running {
    /// Observed by the service thread: farewell-flush a connected peer, then exit (the
    /// socket closes with the host).
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

/// Session owner's grants from [`AppState::access`]. The control thread is the only
/// reader/writer, so a plain `u32` stands in for the native plane's `Arc<AtomicU32>`.
/// Resolve at session start, fold console edits via the watch (within one 2 ms tick),
/// one mask test per event; the wall-clock deadline cuts the session.
struct SessionAccess {
    /// Launch-owner fingerprint (lowercase hex). A different owner re-resolves from scratch.
    fp_hex: String,
    /// Console edits (`NativePairing::subscribe`), polled per tick. `None` in tests: the
    /// mask stays ungoverned-full.
    rx: Option<tokio::sync::watch::Receiver<crate::native_pairing::AccessState>>,
    mask: u32,
    /// Host-wall-clock expiry, unix seconds; `None` = permanent. Checked each tick.
    deadline: Option<i64>,
    /// The grants record was deleted while this session was live. Terminal, like expiry.
    revoked: bool,
}

impl SessionAccess {
    /// Subscribe first, then fold the channel's current value, so a console edit racing
    /// this resolution lands in the borrow or as the first change — never in the gap.
    fn resolve(
        registry: Option<&Arc<crate::native_pairing::NativePairing>>,
        fp_hex: String,
    ) -> SessionAccess {
        let mut access = SessionAccess {
            fp_hex,
            rx: None,
            mask: GRANT_ALL,
            deadline: None,
            revoked: false,
        };
        if let Some(np) = registry {
            let rx = np.subscribe(&access.fp_hex);
            let st = *rx.borrow();
            access.rx = Some(rx);
            // No record at session start is ungoverned (full control): pairing authority
            // is the GameStream cert list. Only a record that exists governs.
            if !st.revoked {
                access.fold(st);
            }
        }
        access
    }

    /// Fold one watch state. A record applies its mask and deadline. `revoked` here means
    /// the record was deleted under a live session: that ends it, as on the native plane.
    /// Nothing but a console edit may widen a live session (per-client-access §6.7).
    fn fold(&mut self, st: crate::native_pairing::AccessState) {
        self.revoked = st.revoked;
        self.mask = if st.revoked { 0 } else { st.grants };
        self.deadline = st.deadline_unix;
    }

    /// Fold a pending watch edit. Non-blocking: the control thread is not async.
    fn poll(&mut self) {
        if let Some(rx) = self.rx.as_mut() {
            if rx.has_changed().unwrap_or(false) {
                let st = *rx.borrow_and_update();
                self.fold(st);
            }
        }
    }

    /// True once `now` is at or past the deadline (that second itself is expired, matching
    /// the trust store's `effective`), or once the record was deleted while live. An
    /// "expire now" edit is a past deadline on the watch.
    fn expired(&self, now_unix: i64) -> bool {
        self.revoked || self.deadline.is_some_and(|d| now_unix >= d)
    }
}

/// Per-(session, grant-class) drop counts. One `warn!` per class for the whole session
/// (per-event logging is a DoS); totals at session end. Plain integers: this thread is
/// the only writer and reader.
struct GrantDrops {
    // One slot per grant bit (7 with `Power`). Power never drops input, but `idx` must
    // stay in bounds for every `GrantClass`.
    counts: [u64; 7],
    warned: [bool; 7],
}

impl GrantDrops {
    fn new() -> GrantDrops {
        GrantDrops {
            counts: [0; 7],
            warned: [false; 7],
        }
    }

    /// Slot in the fixed tables: the grant's bit position, so layout cannot drift from the wire.
    fn idx(class: GrantClass) -> usize {
        class.bit().trailing_zeros() as usize
    }

    /// Count one drop; log only the first of each class. Moonlight has no grants UX, so
    /// that first warn is the only support signal.
    fn note(&mut self, class: GrantClass) {
        let i = Self::idx(class);
        self.counts[i] += 1;
        if !self.warned[i] {
            self.warned[i] = true;
            tracing::warn!(
                class = ?class,
                "gamestream: dropping client input this session's access grants don't cover — \
                 counted; further drops of this class are silent until the session-end totals"
            );
        }
    }

    /// Log drop totals (if any) and reset. Call from every teardown arm.
    fn end_of_session(&mut self) {
        use std::fmt::Write;
        let mut out = String::new();
        for class in [
            GrantClass::Gamepad,
            GrantClass::Pointer,
            GrantClass::Keyboard,
            GrantClass::Clipboard,
            GrantClass::Mic,
            GrantClass::Launch,
        ] {
            let n = self.counts[Self::idx(class)];
            if n != 0 {
                if !out.is_empty() {
                    out.push(' ');
                }
                let _ = write!(out, "{class:?}={n}");
            }
        }
        if !out.is_empty() {
            tracing::info!(drops = %out, "gamestream: access-grant drop totals for the session");
        }
        *self = GrantDrops::new();
    }
}

/// Mask test between a decoded event class and its injector. Free function so tests
/// exercise the same filter the session runs.
fn permitted(mask: u32, class: GrantClass, drops: &mut GrantDrops) -> bool {
    if mask & class.bit() != 0 {
        return true;
    }
    drops.note(class);
    false
}

/// The virtual Xbox pad this session presents, and the only place this plane picks a backend.
///
/// Windows has two, and they are not interchangeable: XUSB registers only
/// `GUID_DEVINTERFACE_XUSB` and has no HID collection, so hidapi/SDL/RawInput/DirectInput/
/// `joy.cpl`/WGI cannot see it — only `XInputGetState`. Both planes read
/// `native::gamepad::windows_xbox_hid` (`cfg(windows)`, so not an intra-doc link): HID where
/// `xinputhid` exists, XUSB where it does not, `PUNKTFUNK_XBOX_BACKEND` overriding both.
///
/// Elsewhere there is no choice: Linux is one uinput X-Box pad; other platforms drop events.
enum SessionPads {
    /// Linux uinput / the Windows XUSB companion — `crate::inject::gamepad`.
    Xusb(GamepadManager),
    /// Windows UMDF HID Xbox pad — the native plane's default.
    #[cfg(target_os = "windows")]
    Hid(crate::inject::xbox_windows::XboxWindowsManager),
}

impl SessionPads {
    fn new() -> SessionPads {
        #[cfg(target_os = "windows")]
        if crate::native::gamepad::windows_xbox_hid() {
            return SessionPads::Hid(crate::inject::xbox_windows::XboxWindowsManager::new());
        }
        SessionPads::Xusb(GamepadManager::new())
    }

    /// Apply one decoded controller event (create/destroy by mask, then state).
    fn handle(&mut self, ev: &GamepadEvent) {
        match self {
            SessionPads::Xusb(m) => m.handle(ev),
            #[cfg(target_os = "windows")]
            SessionPads::Hid(m) => m.handle(ev),
        }
    }

    /// Pump force-feedback every tick: games block inside the kernel handshake until answered.
    /// HID rich-feedback is discarded — GameStream's rumble (`0x010B`) carries the two handle
    /// motors only, so trigger levels are dropped at the call site.
    fn pump_rumble(&mut self, rumble: impl FnMut(u16, u16, u16, u16, u16)) {
        match self {
            SessionPads::Xusb(m) => m.pump_rumble(rumble),
            #[cfg(target_os = "windows")]
            SessionPads::Hid(m) => m.pump(rumble, |_| {}),
        }
    }
}

/// Bind 47999 while any pairing exists, close it when none remain. Call wherever the
/// paired list changes (startup, pairing phase 4, unpair); race-free via [`Gate::running`].
pub(crate) fn sync(state: &Arc<AppState>) -> Result<()> {
    let gate = &state.control_gate;
    if !gate.enabled.load(Ordering::SeqCst) {
        return Ok(());
    }
    let mut slot = gate.running.lock().unwrap_or_else(|e| e.into_inner());
    // Reap a dead listener: a panic would leave a `Running` that serves nobody and
    // blocks every future rebind.
    if slot.as_ref().is_some_and(|r| r.thread.is_finished()) {
        if let Some(r) = slot.take() {
            let _ = r.thread.join();
        }
    }
    let want = !state
        .paired
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty();
    match (slot.is_some(), want) {
        (false, true) => {
            *slot = Some(spawn(state.clone())?);
            Ok(())
        }
        (true, false) => {
            let r = slot.take().expect("slot checked non-empty");
            r.stop.store(true, Ordering::SeqCst);
            // Join so the socket is closed before a re-pair rebinds. Bounded: the loop
            // ticks every 2 ms, plus ~100 ms farewell flush if a client was connected.
            let _ = r.thread.join();
            tracing::info!(
                port = CONTROL_PORT,
                "ENet control torn down — no paired clients remain"
            );
            Ok(())
        }
        _ => Ok(()),
    }
}

/// [`rusty_enet::Socket`] that drops datagrams whose source IP is not the launch owner's.
///
/// rusty_enet 0.4.0 has no setter for `maximum_waiting_data` (C default 32 MiB of per-peer
/// reassembly), so an unauthenticated LAN peer can pin 32 MiB × `peer_limit` and occupy
/// slots. Filter here, before ENet allocates per-peer state.
///
/// The launch is read live: no `/launch` → drop every datagram (nothing on this port is
/// legitimate yet; `accept_connect` agrees). With a launch, only the owner's IP passes.
struct OwnerFilteredSocket {
    inner: UdpSocket,
    state: Arc<AppState>,
}

impl rusty_enet::Socket for OwnerFilteredSocket {
    type Address = std::net::SocketAddr;
    type Error = std::io::Error;

    fn init(&mut self, opts: rusty_enet::SocketOptions) -> Result<(), std::io::Error> {
        rusty_enet::Socket::init(&mut self.inner, opts)?;
        // Blocking socket, 2 ms read timeout, re-asserted after inner init (which may set
        // nonblocking). Idle tick is the empty receive; an arriving datagram wakes immediately.
        // `receive` maps the timeout back to the non-blocking contract rusty_enet expects.
        self.inner.set_nonblocking(false)?;
        self.inner.set_read_timeout(Some(Duration::from_millis(2)))
    }

    fn send(&mut self, address: Self::Address, buffer: &[u8]) -> Result<usize, std::io::Error> {
        rusty_enet::Socket::send(&mut self.inner, address, buffer)
    }

    fn receive(
        &mut self,
        buffer: &mut [u8; rusty_enet::MTU_MAX],
    ) -> Result<Option<(Self::Address, rusty_enet::PacketReceived)>, std::io::Error> {
        // Loop so a dropped non-owner datagram does not starve a following owner one.
        // Timeout is `WouldBlock` on Unix and `TimedOut` on Windows; both map to `Ok(None)`.
        loop {
            match rusty_enet::Socket::receive(&mut self.inner, buffer) {
                Ok(Some((addr, received))) => {
                    // Decide before rusty_enet allocates per-peer reassembly. No live
                    // launch → drop; launch with a known owner IP → keep only that IP.
                    let launch = *self.state.launch.lock().unwrap();
                    match launch.map(|s| s.peer_ip) {
                        None => continue,
                        Some(Some(ip)) if ip != addr.ip() => continue,
                        _ => return Ok(Some((addr, received))),
                    }
                }
                Ok(None) => return Ok(None),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None)
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// Admit a fresh ENet connect only behind a live `/launch` (Moonlight connects after
/// RTSP, so there is no legitimate connect without one). When both sides captured the
/// launching IP it must match, same bind as `rtsp::authorized_launch`. Free function so
/// tests exercise the same gate the session runs.
fn accept_connect(launch: Option<LaunchSession>, from: Option<IpAddr>) -> bool {
    match (launch.map(|l| l.peer_ip), from) {
        (None, _) => false,
        (Some(Some(want)), Some(got)) => want == got,
        // Address unknown on one side → launch-present only.
        _ => true,
    }
}

fn spawn(state: Arc<AppState>) -> Result<Running> {
    let socket = UdpSocket::bind(("0.0.0.0", CONTROL_PORT)).context("bind control UDP")?;
    // Blocking-with-timeout, not nonblocking: `Host::new` calls `init`, which installs
    // the 2 ms read timeout that wakes the service loop on packet arrival.
    let mut host = Host::new(
        OwnerFilteredSocket {
            inner: socket,
            state: state.clone(),
        },
        HostSettings {
            peer_limit: 4,
            // Moonlight uses CTRL_CHANNEL_COUNT (0x30) and sends gamepad on 0x10+n.
            // A smaller limit silently discards controller input.
            channel_limit: 0x30,
            ..Default::default()
        },
    )
    .map_err(|e| anyhow!("ENet host init: {e:?}"))?;
    tracing::info!(port = CONTROL_PORT, "ENet control listening");

    let stop = Arc::new(AtomicBool::new(false));
    let stop_seen = stop.clone();
    let thread = std::thread::Builder::new()
        .name("punktfunk-control".into())
        .spawn(move || {
            let mut detected: Option<Scheme> = None;
            // Consecutive decrypt failures for this peer; throttles the warn so a junk
            // flood cannot spam unbounded lines.
            let mut decrypt_fails: u64 = 0;
            // Keyboard/mouse goes to a host-lifetime injector thread, never inline: a slow
            // Wayland/libei/SendInput must not head-block ENet keepalive. The `inj_tx` clone
            // keeps `InjectorService` (non-Send compositor state) alive for this thread.
            let inj_tx = crate::inject::InjectorService::start().sender();
            let mut pads = SessionPads::new();
            // SS_PEN/SS_TOUCH → tablet / wire touch. Clients send these only after seeing
            // `SS_FF_PEN_TOUCH_EVENTS` (rtsp.rs).
            let mut pointer = super::pen::GsPointer::new();
            // One host→client seq for every outbound message (rumble + HDR). The GCM nonce
            // is derived from `seq`; a per-type counter would reuse (key, nonce) pairs.
            let mut host_seq: u32 = 0;
            // What the client last heard over HDR-mode (0x010e). A client starts in SDR.
            let mut hdr_signalled: Option<HdrMeta> = None;
            let mut peer: Option<PeerID> = None;
            // Last live GCM key. Ending a session clears `launch` (where the key lives), so
            // without this copy the termination that must go out because it ended cannot seal.
            let mut last_key: Option<[u8; 16]> = None;
            // Grant mask + deadline for the launch owner; `None` while no session is live.
            let mut access: Option<SessionAccess> = None;
            let mut drops = GrantDrops::new();
            loop {
                // Last pairing removed while live. Send termination + disconnect (same
                // farewell as host-side session end), flush so it reaches the wire, then
                // exit. Dropping `host` closes the socket.
                if stop_seen.load(Ordering::SeqCst) {
                    if let Some(pid) = peer {
                        if let (Some(scheme), Some(key)) = (detected, last_key) {
                            let pt = termination_plaintext();
                            let wire = encrypt_control(&key, &scheme, host_seq, &pt);
                            if let Err(e) = host.peer_mut(pid).send(0, &Packet::reliable(&wire[..]))
                            {
                                tracing::warn!(error = ?e, "control: termination send failed");
                            }
                        }
                        host.peer_mut(pid).disconnect_later(0);
                        // ~100 ms (50 × 2 ms timeout) for ENet to emit termination and the
                        // disconnect handshake. Each empty receive already blocks 2 ms.
                        for _ in 0..50 {
                            while matches!(host.service(), Ok(Some(_))) {}
                        }
                    }
                    drops.end_of_session();
                    state.end_session("control stream stopped — last pairing removed");
                    tracing::info!(port = CONTROL_PORT, "control: stopped (no paired clients)");
                    return;
                }
                // A stolen display ends the whole session here; the host-side-ended arm
                // below then tells the client.
                state.end_if_preempted();
                // Each 2 ms tick: resolve on a new owner, fold a console edit (watch poll),
                // cut the session the tick the deadline passes. Events below read this mask.
                let owner_fp = state.launch.lock().unwrap().and_then(|s| s.owner_fp);
                match owner_fp {
                    None => access = None,
                    Some(fp) => {
                        let fp_hex = hex::encode(fp);
                        if access.as_ref().is_none_or(|a| a.fp_hex != fp_hex) {
                            access = Some(SessionAccess::resolve(state.access.get(), fp_hex));
                        } else if let Some(a) = access.as_mut() {
                            a.poll();
                        }
                        if let Some(a) = access
                            .as_ref()
                            .filter(|a| a.expired(super::wall_unix_now()))
                        {
                            // Expiry ends the session as a decision, not a network drop.
                            // `quit_session` clears `launch`; the host-side-ended arm then
                            // sends TERMINATION + disconnect (GameStream has no AccessUpdate).
                            let why = if a.revoked {
                                "gamestream access record removed"
                            } else {
                                "gamestream access expired"
                            };
                            tracing::info!(reason = why, "gamestream: ending the session");
                            state.quit_session(why);
                            access = None;
                        }
                    }
                }
                loop {
                    match host.service() {
                        Ok(Some(event)) => match event {
                            Event::Connect { peer: p, .. } => {
                                // Admit only the launch owner. The tracked peer's disconnect
                                // ends the session, so a refused peer is `disconnect_now` (no
                                // `Disconnect` event, slot freed this tick) — leaving it
                                // untracked would pin 32 MiB and one of four slots forever.
                                let launch = *state.launch.lock().unwrap();
                                let from = p.address().map(|a| a.ip());
                                if accept_connect(launch, from) {
                                    tracing::info!("control: client connected");
                                    peer = Some(p.id());
                                } else {
                                    tracing::warn!(
                                        ?from,
                                        "control: peer connected without a matching /launch — refusing"
                                    );
                                    p.disconnect_now(0);
                                }
                            }
                            Event::Disconnect { peer: p, .. } => {
                                // Only the tracked session peer. A probe, or the old peer's
                                // late timeout after a reconnect replaced it, must not end
                                // the live session or clobber its input state.
                                if peer != Some(p.id()) {
                                    tracing::debug!("control: non-session peer disconnected");
                                    continue;
                                }
                                tracing::info!("control: client disconnected");
                                detected = None;
                                decrypt_fails = 0;
                                peer = None;
                                hdr_signalled = None;
                                // Drop pads + tablet: destroying the uinput pen releases any
                                // held tool/tip kernel-side.
                                pads = SessionPads::new();
                                pointer = super::pen::GsPointer::new();
                                drops.end_of_session();
                                // This stream is the session's liveness. Moonlight holds it
                                // for the whole stream; a quit or drop often sends no RTSP
                                // TEARDOWN / `/cancel`. UDP send only errors on ICMP, so
                                // without `end_session` media would stream at a dead peer.
                                state.end_session("control stream disconnected");
                            }
                            Event::Receive {
                                peer: p,
                                channel_id,
                                packet,
                            } => {
                                // Honor only the tracked peer. The socket filter drops
                                // non-owners once a launch is recorded; this covers the
                                // window before the owner IP is captured.
                                if peer != Some(p.id()) {
                                    continue;
                                }

                                // Missing SessionAccess → GRANT_ALL. Input decrypts only
                                // under the `/launch` key, so this is the ≤2 ms sliver
                                // before the next tick's resolve (or an ungoverned session).
                                on_receive(
                                    &state,
                                    channel_id,
                                    packet.data(),
                                    &mut detected,
                                    &mut decrypt_fails,
                                    &inj_tx,
                                    &mut pads,
                                    &mut pointer,
                                    access.as_ref().map(|a| a.mask).unwrap_or(GRANT_ALL),
                                    &mut drops,
                                );
                            }
                        },
                        Ok(None) => break,
                        Err(e) => {
                            tracing::warn!(error = ?e, "control: service error");
                            break;
                        }
                    }
                }
                // Host-side end (`end_session` cleared `launch`): media going silent is not
                // a signal. Send TERMINATION first — a bare disconnect reads as `-1` on the
                // client — then `disconnect_later`. Clearing `peer` first makes this fire
                // once; the real `Disconnect` then takes the non-session-peer branch.
                if let Some(pid) = peer {
                    let ended = state
                        .launch
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_none();
                    if ended {
                        // Seal only once the scheme is known; otherwise fall through to a
                        // bare disconnect rather than send a packet the client cannot read.
                        if let (Some(scheme), Some(key)) = (detected, last_key) {
                            let pt = termination_plaintext();
                            let wire = encrypt_control(&key, &scheme, host_seq, &pt);
                            host_seq = host_seq.wrapping_add(1);
                            if let Err(e) = host.peer_mut(pid).send(0, &Packet::reliable(&wire[..]))
                            {
                                tracing::warn!(error = ?e, "control: termination send failed");
                            }
                        }
                        tracing::info!("control: the session ended — telling the client");
                        // `disconnect_later` flushes the queued termination first; a plain
                        // `disconnect` would race it off the wire.
                        host.peer_mut(pid).disconnect_later(0);
                        peer = None;
                        detected = None;
                        decrypt_fails = 0;
                        hdr_signalled = None;
                        pads = SessionPads::new();
                        pointer = super::pen::GsPointer::new();
                        drops.end_of_session();
                    }
                }
                // Pump FF every tick (games block in EVIOCSFF until answered). Legacy GCM
                // nonces have no direction byte, so host `host_seq` and client seq share
                // (key, nonce) space on collision — inherent to the old wire; V2 separates
                // them with `iv[10..12]`. Do not invent a per-type host counter to "fix" it.
                if let (Some(pid), Some(scheme)) = (peer, detected) {
                    let key = state.launch.lock().unwrap().map(|s| s.gcm_key);
                    // Remember for the teardown message (see `last_key`).
                    if key.is_some() {
                        last_key = key;
                    }
                    if let Some(key) = key {
                        let mut out: Vec<Vec<u8>> = Vec::new();
                        // HDR-mode (0x010e / `IDX_HDR_MODE`) follows the frames the video thread
                        // encodes, off again when they turn SDR. Stock Moonlight switches the
                        // TV only on this cue. Sent before rumble so the client sees it first.
                        let encoded_hdr = *state.video_hdr.lock().unwrap();
                        if encoded_hdr != hdr_signalled {
                            let meta = encoded_hdr.or(hdr_signalled).unwrap_or_default();
                            let pt = hdr_mode_plaintext(encoded_hdr.is_some(), &meta);
                            out.push(encrypt_control(&key, &scheme, host_seq, &pt));
                            host_seq = host_seq.wrapping_add(1);
                            tracing::info!(
                                on = encoded_hdr.is_some(),
                                "control: signaled HDR mode to client (0x010e)"
                            );
                            hdr_signalled = encoded_hdr;
                        }
                        // Handle motors only. `0x010B` has no trigger-rumble id on this
                        // plane, and uinput `FF_RUMBLE` has two fields anyway.
                        pads.pump_rumble(|index, low, high, _lt, _rt| {
                            let pt = super::gamepad::rumble_plaintext(index, low, high);
                            out.push(encrypt_control(&key, &scheme, host_seq, &pt));
                            host_seq = host_seq.wrapping_add(1);
                        });
                        for wire in out {
                            if let Err(e) = host.peer_mut(pid).send(0, &Packet::reliable(&wire[..]))
                            {
                                tracing::warn!(error = ?e, "control send failed");
                            }
                        }
                    }
                } else {
                    // No client/scheme yet: still answer FF uploads so games do not block.
                    pads.pump_rumble(|_, _, _, _, _| {});
                }
                // ENet handshake/keepalive/retransmit pacing is the socket's 2 ms read
                // timeout in the drain above. Do not sleep on top of it.
            }
        })
        .context("spawn control thread")?;
    Ok(Running { stop, thread })
}

/// Lost-frame range from invalidate-reference-frames (0x0301): two LE `i64`
/// (firstFrame, lastFrame) after `[u16 type][u16 length]`, matching
/// `IDX_INVALIDATE_REF_FRAMES`. `None` if short or nonsensical → caller does a full IDR.
fn decode_rfi_range(pt: &[u8]) -> Option<(i64, i64)> {
    if pt.len() < 20 {
        return None;
    }
    let first = i64::from_le_bytes(pt[4..12].try_into().ok()?);
    let last = i64::from_le_bytes(pt[12..20].try_into().ok()?);
    (first >= 0 && last >= first).then_some((first, last))
}

/// Decrypt one control packet (lock GCM scheme on the first authenticating one),
/// classify against the session grant mask, inject what the grants cover.
#[allow(clippy::too_many_arguments)]
fn on_receive(
    state: &AppState,
    _channel_id: u8,
    d: &[u8],
    detected: &mut Option<Scheme>,
    decrypt_fails: &mut u64,
    inj_tx: &Sender<InputEvent>,
    pads: &mut SessionPads,
    pointer: &mut super::pen::GsPointer,
    grants: u32,
    drops: &mut GrantDrops,
) {
    let Some(key) = state.launch.lock().unwrap().map(|s| s.gcm_key) else {
        return; // control traffic before /launch — no key yet
    };
    // Encrypted control packets begin with u16 LE encType = 0x0001 and an 8-byte header.
    if d.len() < 8 || d[0] != 0x01 || d[1] != 0x00 {
        return;
    }

    let pt = match decrypt_control(&key, d, detected) {
        Some((scheme, pt)) => {
            if detected.is_none() {
                tracing::info!(?scheme, "control: GCM scheme locked in");
            }
            *detected = Some(scheme);
            *decrypt_fails = 0;
            pt
        }
        None => {
            // Log the first decrypt failure, then only at 2, 4, 8, … — a junk flood
            // must not spam one warn per packet.
            *decrypt_fails += 1;
            if decrypt_fails.is_power_of_two() {
                tracing::warn!(
                    len = d.len(),
                    fails = *decrypt_fails,
                    "control: GCM decrypt failed"
                );
            }
            return;
        }
    };

    // Loss recovery. 0x0301 (Gen7 RFI) carries the lost-frame range for NVENC
    // invalidate-ref; 0x0302 / 0x0305 and a malformed 0x0301 force a keyframe.
    // The video thread drains `rfi_range` / `force_idr`.
    if pt.len() >= 2 {
        let inner = u16::from_le_bytes([pt[0], pt[1]]);
        if inner == 0x0301 {
            if let Some((first, last)) = decode_rfi_range(&pt) {
                *state.rfi_range.lock().unwrap() = Some((first, last));
                tracing::debug!(first, last, "control: RFI request → invalidate ref frames");
            } else {
                state
                    .force_idr
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                tracing::debug!("control: RFI request (no range) → keyframe");
            }
            return;
        }
        if matches!(inner, 0x0302 | 0x0305) {
            state
                .force_idr
                .store(true, std::sync::atomic::Ordering::SeqCst);
            tracing::debug!(
                ty = %format_args!("{inner:#06x}"),
                "control: IDR request → keyframe"
            );
            return;
        }
        // 0x0201 Gen7 loss-stats: LE i32s after [type][len] — [0]=lost, [1]=window ms,
        // [3]=last-good frame (`IDX_LOSS_STATS`). Cumulative; video thread 1 Hz step
        // reads window deltas for FEC + bitrate. Pin: design/research/gamestream-protocol-research.json.
        if inner == 0x0201 && pt.len() >= 20 {
            let lost = i32::from_le_bytes(pt[4..8].try_into().expect("len checked")).max(0);
            let window_ms = i32::from_le_bytes(pt[8..12].try_into().expect("len checked"));
            let last_good = i32::from_le_bytes(pt[16..20].try_into().expect("len checked"));
            state
                .loss_stats
                .lost
                .fetch_add(lost as u64, std::sync::atomic::Ordering::Relaxed);
            state
                .loss_stats
                .reports
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if lost > 0 {
                tracing::debug!(lost, window_ms, last_good, "control: client loss report");
            }
            return;
        }
    }

    // Gate gamepad before the manager sees it: without GAMEPAD the creating event never
    // arrives, so no uinput node and no pad-audio streamer.
    if let Some(gp) = super::gamepad::decode(&pt) {
        crate::sleep_inhibit::note_input();
        if permitted(grants, GrantClass::Gamepad, drops) {
            state.counters.input_rich.fetch_add(1, Ordering::Relaxed);
            pads.handle(&gp);
        }
        return;
    }

    // Pen/touch (only after our feature flag): pen → virtual tablet, touch → wire
    // touches. Pointer-class by the plane tag.
    if let Some(p) = super::input::decode_pointer(&pt) {
        crate::sleep_inhibit::note_input();
        if permitted(grants, GrantClass::Pointer, drops) {
            state.counters.input_rich.fetch_add(1, Ordering::Relaxed);
            pointer.apply(&p, |ev| {
                let _ = inj_tx.send(ev);
            });
        }
        return;
    } else if super::input::is_pointer_magic(&pt) {
        // Pointer magic, body parse failed: layout mismatch. Dump the first few
        // payloads so the log alone diagnoses it.
        static HEX_DUMPS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if HEX_DUMPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 5 {
            let hex: String = pt.iter().map(|b| format!("{b:02x}")).collect();
            tracing::warn!(
                len = pt.len(),
                bytes = %hex,
                "gamestream: malformed SS_TOUCH/SS_PEN packet (unexpected layout)"
            );
        } else {
            tracing::warn!(
                len = pt.len(),
                "gamestream: malformed SS_TOUCH/SS_PEN packet (unexpected layout)"
            );
        }
        return;
    }

    let events = super::input::decode(&pt);
    if events.is_empty() {
        return; // keepalive / QoS / unhandled input kind
    }
    // Real input: lift any suspend veto so the guest's own Sleep reaches logind.
    // Past `is_empty` on purpose — a keepalive is what a passive viewer sends.
    crate::sleep_inhibit::note_input();

    // One mask test, then the injector thread. A closed channel means the injector
    // died at startup; input is lossy, so drop silently.
    for ev in events {
        if permitted(grants, classify(ev.kind), drops) {
            state.counters.input_events.fetch_add(1, Ordering::Relaxed);
            let _ = inj_tx.send(ev);
        }
    }
}

/// How a control packet's nonce is built. Moonlight picks one from the negotiated flags.
#[derive(Clone, Copy, Debug)]
enum NonceKind {
    /// `SS_ENC_CONTROL_V2`: 12-byte nonce, `seq` in [0..4], marker bytes at [10..12].
    V2 { seq_be: bool, marker: [u8; 2] },
    /// Legacy: 16-byte nonce, only `iv[0] = seq & 0xff` (the rest zero).
    LegacyLowByte,
    /// Legacy variant: 16-byte nonce, full `seq` in [0..4] (the rest zero).
    Legacy16Seq { seq_be: bool },
}

impl NonceKind {
    fn nonce(&self, seq: u32) -> Vec<u8> {
        let seq_bytes = |be: bool| {
            if be {
                seq.to_be_bytes()
            } else {
                seq.to_le_bytes()
            }
        };
        match *self {
            NonceKind::V2 { seq_be, marker } => {
                let mut iv = vec![0u8; 12];
                iv[0..4].copy_from_slice(&seq_bytes(seq_be));
                iv[10] = marker[0];
                iv[11] = marker[1];
                iv
            }
            NonceKind::LegacyLowByte => {
                let mut iv = vec![0u8; 16];
                iv[0] = (seq & 0xff) as u8;
                iv
            }
            NonceKind::Legacy16Seq { seq_be } => {
                let mut iv = vec![0u8; 16];
                iv[0..4].copy_from_slice(&seq_bytes(seq_be));
                iv
            }
        }
    }
}

/// GCM scheme that opened a control packet. Locked once per connection: AES-GCM gives
/// no partial credit, so an authenticating combination is proof.
#[derive(Clone, Copy, Debug)]
struct Scheme {
    /// `gcm_key` is byte-reversed before use (defensive; Sunshine's net effect is forward).
    key_rev: bool,
    nonce: NonceKind,
    /// GCM tag sits before the ciphertext (vs after).
    tag_first: bool,
    aad: Aad,
}

#[derive(Clone, Copy, Debug)]
enum Aad {
    None,
    /// The 4-byte cleartext header prefix (encType + length), `d[0..4]`.
    Header4,
}

impl Scheme {
    fn key(&self, base: &[u8; 16]) -> [u8; 16] {
        let mut k = *base;
        if self.key_rev {
            k.reverse();
        }
        k
    }
}

/// Open encrypted control packet `d` (8-byte cleartext header + `[tag?][ciphertext]`).
/// Fast path: only `detected`. Otherwise sweep nonce × key order × tag position × AAD
/// and return the combination whose GCM tag authenticates.
fn decrypt_control(
    key: &[u8; 16],
    d: &[u8],
    detected: &Option<Scheme>,
) -> Option<(Scheme, Vec<u8>)> {
    let seq = u32::from_le_bytes([d[4], d[5], d[6], d[7]]);
    let payload = &d[8..];
    if payload.len() < 16 {
        return None;
    }

    let attempt = |s: Scheme| -> Option<Vec<u8>> {
        // aes-gcm wants `ciphertext || tag`; reassemble from whichever wire order this is.
        let (ct, tag) = if s.tag_first {
            (&payload[16..], &payload[..16])
        } else {
            (
                &payload[..payload.len() - 16],
                &payload[payload.len() - 16..],
            )
        };
        let mut ct_tag = Vec::with_capacity(ct.len() + 16);
        ct_tag.extend_from_slice(ct);
        ct_tag.extend_from_slice(tag);
        let aad: &[u8] = match s.aad {
            Aad::None => &[],
            Aad::Header4 => &d[0..4],
        };
        gcm_open(&s.key(key), &s.nonce.nonce(seq), &ct_tag, aad)
    };

    if let Some(s) = *detected {
        return attempt(s).map(|pt| (s, pt));
    }

    // Candidate nonce constructions, most-likely first.
    const MARKERS: [[u8; 2]; 3] = [*b"CC", *b"HC", *b"CH"];
    let mut kinds: Vec<NonceKind> = vec![NonceKind::LegacyLowByte];
    for seq_be in [false, true] {
        for marker in MARKERS {
            kinds.push(NonceKind::V2 { seq_be, marker });
        }
        kinds.push(NonceKind::Legacy16Seq { seq_be });
    }

    for &nonce in &kinds {
        for key_rev in [false, true] {
            for tag_first in [true, false] {
                for aad in [Aad::None, Aad::Header4] {
                    let s = Scheme {
                        key_rev,
                        nonce,
                        tag_first,
                        aad,
                    };
                    if let Some(pt) = attempt(s) {
                        return Some((s, pt));
                    }
                }
            }
        }
    }
    None
}

/// Moonlight `SS_HDR_METADATA`: 26 bytes, little-endian (`BYTE_ORDER_LITTLE`).
/// Primaries are R, G, B on the wire; [`HdrMeta`]/ST.2086 stores G, B, R. Luminance:
/// `maxDisplayLuminance`/`maxFullFrameLuminance` in whole nits, `minDisplayLuminance`
/// in 1/10000-nit; content light levels already nits. No separate full-frame value, so
/// it mirrors the mastering peak.
fn ss_hdr_metadata(m: &HdrMeta) -> [u8; 26] {
    let max_display_nits = (m.max_display_mastering_luminance / 10_000).min(u16::MAX as u32) as u16;
    let min_display = m.min_display_mastering_luminance.min(u16::MAX as u32) as u16;
    let mut b = [0u8; 26];
    let mut o = 0;
    let mut put = |v: u16| {
        b[o..o + 2].copy_from_slice(&v.to_le_bytes());
        o += 2;
    };
    // displayPrimaries[3] in R, G, B order (HdrMeta is G, B, R).
    for p in [
        m.display_primaries[2],
        m.display_primaries[0],
        m.display_primaries[1],
    ] {
        put(p[0]);
        put(p[1]);
    }
    put(m.white_point[0]);
    put(m.white_point[1]);
    put(max_display_nits); // maxDisplayLuminance (nits)
    put(min_display); // minDisplayLuminance (1/10000 nit)
    put(m.max_cll); // maxContentLightLevel (nits)
    put(m.max_fall); // maxFrameAverageLightLevel (nits)
    put(max_display_nits); // maxFullFrameLuminance (nits) — no separate value; mirror the peak
    debug_assert_eq!(o, 26);
    b
}

/// Host→client HDR-mode plaintext (`0x010e` / `IDX_HDR_MODE`):
/// `[u16 type][u16 length][u8 enabled][SS_HDR_METADATA]`, LE, `length` = enable + metadata.
/// Moonlight flips HDR picture mode on `enabled != 0`. We advertise Sunshine, so the
/// client (`IS_SUNSHINE()`) reads the full 26-byte metadata block.
fn hdr_mode_plaintext(enabled: bool, m: &HdrMeta) -> Vec<u8> {
    let meta = ss_hdr_metadata(m);
    let mut pt = Vec::with_capacity(4 + 1 + meta.len());
    pt.extend_from_slice(&0x010eu16.to_le_bytes());
    pt.extend_from_slice(&((1 + meta.len()) as u16).to_le_bytes()); // length = enable + metadata
    pt.push(enabled as u8);
    pt.extend_from_slice(&meta);
    pt
}

/// Host→client TERMINATION: the session ended on purpose. Without it, media going
/// silent looks like the host fell over (frozen last frame, or `-1` after disconnect).
///
/// Type `0x0109` from `packetTypesGen7Enc[IDX_TERMINATION]`. The client picks that table
/// iff `APP_VERSION_AT_LEAST(7, 1, 431)`; [`super::APP_VERSION`] is exactly `7.1.431`.
/// Do not derive the type from [`NonceKind`] — that sent `0x0100` (plain table) to a
/// client on the encrypted table, which ignored it. Pin: moonlight-common-c `ControlStream.c`.
///
/// Payload is a big-endian `u32` (extended ≥6-byte branch; the short branch is LE `u16`).
/// `0x80030023` is `NVST_DISCONN_SERVER_TERMINATED_CLOSED` → `ML_ERROR_GRACEFUL_TERMINATION`
/// once a frame has been seen.
fn termination_plaintext() -> Vec<u8> {
    /// `packetTypesGen7Enc[IDX_TERMINATION]` — see the version gate above.
    const TERMINATION: u16 = 0x0109;
    /// `NVST_DISCONN_SERVER_TERMINATED_CLOSED` — a deliberate, graceful host-side end.
    const GRACEFUL: u32 = 0x8003_0023;
    let mut pt = Vec::with_capacity(8);
    pt.extend_from_slice(&TERMINATION.to_le_bytes());
    pt.extend_from_slice(&4u16.to_le_bytes()); // length = the reason that follows
    pt.extend_from_slice(&GRACEFUL.to_be_bytes()); // big-endian: the extended branch
    pt
}

/// Seal a host→client control message on the client's `detected` scheme, direction
/// flipped: V2 markers `H?` instead of `C?`; legacy keeps its construction with our
/// independent `seq`. Wire: `[0x0001][length][seq][tag|ct per scheme.tag_first]`.
fn encrypt_control(key: &[u8; 16], scheme: &Scheme, seq: u32, pt: &[u8]) -> Vec<u8> {
    let nonce_kind = match scheme.nonce {
        NonceKind::V2 { seq_be, marker } => NonceKind::V2 {
            seq_be,
            marker: [b'H', marker[1]],
        },
        other => other,
    };
    let length = (4 + 16 + pt.len()) as u16;
    let mut wire = Vec::with_capacity(8 + 16 + pt.len());
    wire.extend_from_slice(&0x0001u16.to_le_bytes());
    wire.extend_from_slice(&length.to_le_bytes());
    wire.extend_from_slice(&seq.to_le_bytes());
    let aad: Vec<u8> = match scheme.aad {
        Aad::None => Vec::new(),
        Aad::Header4 => wire[0..4].to_vec(),
    };
    let ct_tag = gcm_seal(&scheme.key(key), &nonce_kind.nonce(seq), pt, &aad);
    let (ct, tag) = ct_tag.split_at(ct_tag.len() - 16);
    if scheme.tag_first {
        wire.extend_from_slice(tag);
        wire.extend_from_slice(ct);
    } else {
        wire.extend_from_slice(ct);
        wire.extend_from_slice(tag);
    }
    wire
}

/// AES-128-GCM seal (companion to [`gcm_open`]); returns `ciphertext || tag`. Shared
/// with the RTSP plane under the same session key.
pub(super) fn gcm_seal(key: &[u8; 16], nonce: &[u8], pt: &[u8], aad: &[u8]) -> Vec<u8> {
    use aes_gcm::aead::consts::{U12, U16};
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{aes::Aes128, AesGcm};

    let p = Payload { msg: pt, aad };
    // Each arm's `try_into` is guarded by the length it matched on.
    match nonce.len() {
        12 => AesGcm::<Aes128, U12>::new_from_slice(key)
            .unwrap()
            .encrypt(nonce.try_into().expect("12-byte nonce"), p)
            .expect("GCM seal"),
        16 => AesGcm::<Aes128, U16>::new_from_slice(key)
            .unwrap()
            .encrypt(nonce.try_into().expect("16-byte nonce"), p)
            .expect("GCM seal"),
        _ => unreachable!("nonce length"),
    }
}

/// AES-128-GCM open, 12- or 16-byte nonce, explicit AAD. Plaintext iff the tag
/// authenticates. `ct_tag` is `ciphertext || tag` (aes-gcm's order).
pub(super) fn gcm_open(key: &[u8; 16], nonce: &[u8], ct_tag: &[u8], aad: &[u8]) -> Option<Vec<u8>> {
    use aes_gcm::aead::consts::{U12, U16};
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{aes::Aes128, AesGcm};

    let p = Payload { msg: ct_tag, aad };
    match nonce.len() {
        12 => AesGcm::<Aes128, U12>::new_from_slice(key)
            .ok()?
            .decrypt(nonce.try_into().ok()?, p)
            .ok(),
        16 => AesGcm::<Aes128, U16>::new_from_slice(key)
            .ok()?
            .decrypt(nonce.try_into().ok()?, p)
            .ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::decode_rfi_range;

    /// Build a 0x0301 invalidate-ref-frames plaintext: `[type LE][len LE][firstFrame i64 LE][last i64 LE]`.
    fn rfi_msg(first: i64, last: i64) -> Vec<u8> {
        let mut v = vec![0x01, 0x03, 0x10, 0x00]; // type 0x0301, length 16
        v.extend_from_slice(&first.to_le_bytes());
        v.extend_from_slice(&last.to_le_bytes());
        v
    }

    fn launched(peer_ip: Option<std::net::IpAddr>) -> Option<super::LaunchSession> {
        Some(super::LaunchSession {
            gcm_key: [0; 16],
            rikeyid: 0,
            width: 1920,
            height: 1080,
            fps: 60,
            appid: 1,
            peer_ip,
            owner_fp: None,
        })
    }

    /// 47999 is open the idle life of a paired host: refuse connects with no `/launch`
    /// (they would squat one of four slots), and admit only the owner's IP. Unknown
    /// address on either side → launch-present-only, like `rtsp::authorized_launch`.
    #[test]
    fn connects_are_admitted_only_behind_a_matching_launch() {
        let owner: std::net::IpAddr = "192.168.1.20".parse().unwrap();
        let other: std::net::IpAddr = "192.168.1.99".parse().unwrap();
        assert!(!super::accept_connect(None, Some(owner)));
        assert!(!super::accept_connect(None, None));
        assert!(super::accept_connect(launched(Some(owner)), Some(owner)));
        assert!(!super::accept_connect(launched(Some(owner)), Some(other)));
        // Address unknown on one side → launch-present only.
        assert!(super::accept_connect(launched(Some(owner)), None));
        assert!(super::accept_connect(launched(None), Some(other)));
    }

    #[test]
    fn decodes_a_valid_rfi_range() {
        assert_eq!(decode_rfi_range(&rfi_msg(40, 47)), Some((40, 47)));
        assert_eq!(decode_rfi_range(&rfi_msg(5, 5)), Some((5, 5))); // single frame
    }

    #[test]
    fn rejects_short_or_nonsensical_ranges() {
        assert_eq!(decode_rfi_range(&[0x01, 0x03, 0x00, 0x00]), None); // header only, no body
        assert_eq!(decode_rfi_range(&rfi_msg(-1, 9)), None); // negative first
        assert_eq!(decode_rfi_range(&rfi_msg(9, 4)), None); // last < first
    }

    /// Wrong type table or reason endianness is ignored by the client (frozen stream or
    /// `-1`), not a failed decode. Pin: moonlight-common-c `ControlStream.c`.
    #[test]
    fn termination_plaintext_wire_layout() {
        let pt = super::termination_plaintext();
        assert_eq!(pt.len(), 8);
        // Encrypted table — the one every client at our advertised version reads.
        assert_eq!(&pt[0..2], &0x0109u16.to_le_bytes());
        assert_eq!(&pt[2..4], &4u16.to_le_bytes());
        // Reason is big-endian: the client's ≥6-byte extended branch.
        assert_eq!(&pt[4..8], &0x8003_0023u32.to_be_bytes());
    }

    /// Termination `0x0109` is correct only because we advertise ≥ 7.1.431:
    /// that is `encryptedControlStream`, which selects `packetTypesGen7Enc`. Below it
    /// the client reads `0x0100` and this test would still pass while the stream ends as `-1`.
    #[test]
    fn advertised_version_keeps_the_client_on_the_encrypted_packet_table() {
        let q: Vec<i32> = super::super::APP_VERSION
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect();
        let at_least = q[0] > 7 || (q[0] == 7 && (q[1] > 1 || (q[1] == 1 && q[2] >= 431)));
        assert!(
            at_least,
            "APP_VERSION {} is below 7.1.431, so clients read the PLAIN packet table and \
             termination must become 0x0100",
            super::super::APP_VERSION
        );
    }

    /// The HDR-mode plaintext must match what moonlight-common-c parses: `[u16 type=0x010e]
    /// [u16 length=27][u8 enable][SS_HDR_METADATA]`, 31 bytes, all little-endian, primaries R,G,B.
    #[test]
    fn hdr_mode_plaintext_wire_layout() {
        let pt = super::hdr_mode_plaintext(true, &pf_frame::hdr::generic_hdr10());
        assert_eq!(pt.len(), 31); // 4 header + 1 enable + 26 metadata
        assert_eq!(&pt[0..2], &0x010eu16.to_le_bytes());
        assert_eq!(&pt[2..4], &27u16.to_le_bytes()); // length = enable + metadata
        assert_eq!(pt[4], 1);
        // Metadata starts at byte 5, R primary first (HdrMeta stores G,B,R; wire is R,G,B).
        assert_eq!(&pt[5..7], &35400u16.to_le_bytes()); // red.x
        assert_eq!(&pt[7..9], &14600u16.to_le_bytes()); // red.y
        assert_eq!(&pt[9..11], &8500u16.to_le_bytes()); // green.x
        assert_eq!(&pt[13..15], &6550u16.to_le_bytes()); // blue.x
        assert_eq!(&pt[17..19], &15635u16.to_le_bytes()); // whitePoint.x
        assert_eq!(&pt[21..23], &1000u16.to_le_bytes()); // maxDisplayLuminance (nits)
        assert_eq!(&pt[23..25], &1u16.to_le_bytes()); // minDisplayLuminance (1/10000 nit)
        assert_eq!(&pt[25..27], &1000u16.to_le_bytes()); // maxContentLightLevel (MaxCLL)
        assert_eq!(&pt[27..29], &400u16.to_le_bytes()); // maxFrameAverageLightLevel (MaxFALL)
        assert_eq!(&pt[29..31], &1000u16.to_le_bytes()); // maxFullFrameLuminance mirrors the peak
    }

    #[test]
    fn hdr_mode_plaintext_disabled_still_well_formed() {
        let pt = super::hdr_mode_plaintext(false, &pf_frame::hdr::generic_hdr10());
        assert_eq!(pt.len(), 31);
        assert_eq!(&pt[2..4], &27u16.to_le_bytes());
        assert_eq!(pt[4], 0); // disabled
    }

    /// Controller-only mask: pad injects, keyboard/pointer are counted-and-dropped.
    /// `permitted` is what `on_receive` calls. Also pins the drop-count reset.
    #[test]
    fn controller_only_mask_passes_the_pad_and_drops_keyboard_and_pointer() {
        use punktfunk_core::input::InputKind;
        use punktfunk_core::quic::{classify, GrantClass, GRANT_PRESET_CONTROLLER_ONLY};
        let mut drops = super::GrantDrops::new();
        let mask = GRANT_PRESET_CONTROLLER_ONLY;
        // Pad injects (creation with it — deny-at-setup is upstream of this).
        assert!(super::permitted(
            mask,
            classify(InputKind::GamepadButton),
            &mut drops
        ));
        assert!(!super::permitted(
            mask,
            classify(InputKind::KeyDown),
            &mut drops
        ));
        assert!(!super::permitted(
            mask,
            classify(InputKind::TextInput),
            &mut drops
        ));
        assert!(!super::permitted(
            mask,
            classify(InputKind::MouseMove),
            &mut drops
        ));
        // The pen/touch plane is Pointer-class by its plane tag.
        assert!(!super::permitted(mask, GrantClass::Pointer, &mut drops));
        assert_eq!(
            drops.counts[super::GrantDrops::idx(GrantClass::Keyboard)],
            2
        );
        assert_eq!(drops.counts[super::GrantDrops::idx(GrantClass::Pointer)], 2);
        assert_eq!(drops.counts[super::GrantDrops::idx(GrantClass::Gamepad)], 0);
        drops.end_of_session();
        assert_eq!(drops.counts, [0u64; 7]);
    }

    /// No grants record at session start is ungoverned (full control, back-compat for
    /// existing pairings). A record governs; console edits fold in within one poll; "expire
    /// now" is a past deadline on the same watch. Deleting the record under a live session
    /// ends it: a deletion must never widen a session it was governing.
    #[test]
    fn session_access_resolves_folds_edits_and_expires() {
        use crate::native_pairing::{Access, NativePairing};
        use punktfunk_core::quic::{GRANT_ALL, GRANT_GAMEPAD};
        use std::sync::Arc;
        let x = 0u8;
        let p = std::env::temp_dir().join(format!(
            "pf-gs-session-access-{}-{}.json",
            std::process::id(),
            &x as *const _ as usize
        ));
        let _ = std::fs::remove_file(&p);
        let np = Arc::new(NativePairing::load_with(Some(p.clone()), None, false).unwrap());
        let now = super::super::wall_unix_now();

        // No registry wired (an AppState that never went through `serve`): ungoverned forever.
        let a = super::SessionAccess::resolve(None, "ab12".into());
        assert_eq!(a.mask, GRANT_ALL);
        assert!(!a.expired(now + 1_000_000));

        // Registry wired, no record: ungoverned — a stock Moonlight pairing keeps full control.
        let mut a = super::SessionAccess::resolve(Some(&np), "ab12".into());
        assert_eq!(a.mask, GRANT_ALL);
        assert_eq!(a.deadline, None);

        // Console-created record governs within one watch poll.
        np.add_with_access(
            "Moonlight Deck",
            "AB12", // registry keys case-insensitively, like the store
            Some(Access {
                grants: GRANT_GAMEPAD,
                expires_unix: Some(now + 60),
                until_disconnect: false,
            }),
        )
        .unwrap();
        a.poll();
        assert_eq!(a.mask, GRANT_GAMEPAD);
        assert!(!a.expired(now + 59));
        assert!(a.expired(now + 60), "the deadline second itself is expired");

        // "Expire now" is just a deadline in the past arriving through the same watch.
        np.set_access(
            "ab12",
            Access {
                grants: GRANT_GAMEPAD,
                expires_unix: Some(now - 1),
                until_disconnect: false,
            },
        )
        .unwrap();
        a.poll();
        assert!(a.expired(now));

        // Deleting a governing record ends the session; it never widens it to full.
        np.set_access(
            "ab12",
            Access {
                grants: GRANT_GAMEPAD,
                expires_unix: None,
                until_disconnect: false,
            },
        )
        .unwrap();
        a.poll();
        assert!(!a.expired(now));
        assert!(np.remove("ab12").unwrap());
        a.poll();
        assert_eq!(a.mask, 0);
        assert!(a.revoked);
        assert!(a.expired(now));

        // A session that starts after the deletion is ungoverned again.
        let a = super::SessionAccess::resolve(Some(&np), "ab12".into());
        assert_eq!(a.mask, GRANT_ALL);
        assert!(!a.revoked);
        assert!(!a.expired(now + 1_000_000));
        let _ = std::fs::remove_file(&p);
    }
}
