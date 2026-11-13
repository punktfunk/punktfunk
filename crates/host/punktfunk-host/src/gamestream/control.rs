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

use super::control_wire::{decrypt_control, encrypt_control, ControlMsg, Scheme};
use super::{LaunchSession, CONTROL_PORT};
use crate::events::Plane;
use crate::host::AppState;
use crate::inject::gamepad::GamepadManager;
use crate::session_status::GrantDrops;
use anyhow::{anyhow, Context, Result};
use pf_frame::HdrMeta;
use punktfunk_core::input::{GamepadEvent, GamepadFrame, InputEvent, MAX_PADS};
use punktfunk_core::quic::{
    classify, GrantClass, GRANT_ALL, GRANT_GAMEPAD, GRANT_KEYBOARD, GRANT_POINTER,
};
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

    /// Fold a pending watch edit. Non-blocking: the control thread is not async. Returns the
    /// grant bits the edit withdrew, so the peer lets go of what they covered.
    fn poll(&mut self) -> u32 {
        let before = self.mask;
        if let Some(rx) = self.rx.as_mut() {
            if rx.has_changed().unwrap_or(false) {
                let st = *rx.borrow_and_update();
                self.fold(st);
            }
        }
        before & !self.mask
    }

    /// True once `now` is at or past the deadline (that second itself is expired, matching
    /// the trust store's `effective`), or once the record was deleted while live. An
    /// "expire now" edit is a past deadline on the watch.
    fn expired(&self, now_unix: i64) -> bool {
        self.revoked || self.deadline.is_some_and(|d| now_unix >= d)
    }
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
    let gate = &state.gs.control_gate;
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
        // Keep reading past a dropped non-owner datagram so it cannot starve an owner one, but
        // yield after `MAX_DROPS_PER_RECEIVE` so ENet's ACK and timeout pass still runs.
        // Timeout is `WouldBlock` on Unix and `TimedOut` on Windows; both map to `Ok(None)`.
        for _ in 0..MAX_DROPS_PER_RECEIVE {
            match rusty_enet::Socket::receive(&mut self.inner, buffer) {
                Ok(Some((addr, received))) => {
                    // Decide before rusty_enet allocates per-peer reassembly. No live
                    // launch → drop; launch with a known owner IP → keep only that IP.
                    let launch = *self.state.launch.lock().unwrap_or_else(|e| e.into_inner());
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
        Ok(None)
    }
}

/// Datagrams one [`OwnerFilteredSocket::receive`] reads and drops before it reports none.
/// ENet sends ACKs, checks timeouts and dispatches only once `receive` returns none, and a
/// flood from another host never leaves the 2 ms read timeout to expire.
const MAX_DROPS_PER_RECEIVE: usize = 64;

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
            // Keyboard/mouse goes to a host-lifetime injector thread, never inline: a slow
            // Wayland/libei/SendInput must not head-block ENet keepalive. The `inj_tx` clone
            // keeps `InjectorService` (non-Send compositor state) alive for this thread.
            let inj_tx = crate::inject::InjectorService::start().sender();
            let mut peer = ControlPeer::new();
            // Grant mask + deadline for the launch owner; `None` while no session is live.
            let mut access: Option<SessionAccess> = None;
            loop {
                // Last pairing removed while live: the host-side-end farewell, flushed so it
                // reaches the wire, then exit. Dropping `host` closes the socket.
                if stop_seen.load(Ordering::SeqCst) {
                    if peer.farewell(&mut host) {
                        // ~100 ms (50 × 2 ms timeout) for ENet to emit termination and the
                        // disconnect handshake. Each empty receive already blocks 2 ms.
                        for _ in 0..50 {
                            while matches!(host.service(), Ok(Some(_))) {}
                        }
                    }
                    peer.reset(&inj_tx);
                    state.end_session("control stream stopped — last pairing removed");
                    tracing::info!(port = CONTROL_PORT, "control: stopped (no paired clients)");
                    return;
                }
                // A stolen display ends the whole session here; the host-side-ended arm
                // below then tells the client.
                state.end_if_preempted();
                tick_access(&mut access, &state, &mut peer, &inj_tx);
                loop {
                    match host.service() {
                        Ok(Some(event)) => match event {
                            Event::Connect { peer: p, .. } => {
                                // Admit only the launch owner. The tracked peer's disconnect
                                // ends the session, so a refused peer is `disconnect_now` (no
                                // `Disconnect` event, slot freed this tick) — leaving it
                                // untracked would pin 32 MiB and one of four slots forever.
                                let launch = *state.launch.lock().unwrap_or_else(|e| e.into_inner());
                                let from = p.address().map(|a| a.ip());
                                if accept_connect(launch, from) {
                                    tracing::info!("control: client connected");
                                    // A reconnect that replaces a tracked peer: that peer's
                                    // late Disconnect is ignored below, so release what it held.
                                    if peer.id.is_some_and(|id| id != p.id()) {
                                        peer.reset(&inj_tx);
                                    }
                                    peer.id = Some(p.id());
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
                                if peer.id != Some(p.id()) {
                                    tracing::debug!("control: non-session peer disconnected");
                                    continue;
                                }
                                tracing::info!("control: client disconnected");
                                peer.reset(&inj_tx);
                                // This stream is the session's liveness. Moonlight holds it
                                // for the whole stream; a quit or drop often sends no RTSP
                                // TEARDOWN / `/cancel`. UDP send only errors on ICMP, so
                                // without `end_session` media would stream at a dead peer.
                                state.end_session("control stream disconnected");
                            }
                            Event::Receive {
                                peer: p, packet, ..
                            } => {
                                // Honor only the tracked peer. The socket filter drops
                                // non-owners once a launch is recorded; this covers the
                                // window before the owner IP is captured.
                                if peer.id != Some(p.id()) {
                                    continue;
                                }

                                // Missing SessionAccess → GRANT_ALL. Input decrypts only
                                // under the `/launch` key, so this is the ≤2 ms sliver
                                // before the next tick's resolve (or an ungoverned session).
                                on_receive(
                                    &state,
                                    packet.data(),
                                    &mut peer,
                                    &inj_tx,
                                    access.as_ref().map(|a| a.mask).unwrap_or(GRANT_ALL),
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
                // a signal. Clearing the peer makes this fire once; the real `Disconnect`
                // then takes the non-session-peer branch.
                let ended = state
                    .launch
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_none();
                if ended && peer.farewell(&mut host) {
                    tracing::info!("control: the session ended — telling the client");
                    peer.reset(&inj_tx);
                }
                peer.pump_outbound(&mut host, &state);
                peer.pump_repeat(&inj_tx, access.as_ref().map(|a| a.mask).unwrap_or(GRANT_ALL));
                // ENet handshake/keepalive/retransmit pacing is the socket's 2 ms read
                // timeout in the drain above. Do not sleep on top of it.
            }
        })
        .context("spawn control thread")?;
    Ok(Running { stop, thread })
}

/// Each 2 ms tick: resolve on a new owner, fold a console edit (watch poll), and cut the
/// session the tick the deadline passes. Events read the mask this leaves.
fn tick_access(
    access: &mut Option<SessionAccess>,
    state: &AppState,
    peer: &mut ControlPeer,
    inj_tx: &Sender<InputEvent>,
) {
    let owner_fp = state
        .launch
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .and_then(|s| s.owner_fp);
    let Some(fp) = owner_fp else {
        *access = None;
        return;
    };
    let fp_hex = hex::encode(fp);
    if access.as_ref().is_none_or(|a| a.fp_hex != fp_hex) {
        *access = Some(SessionAccess::resolve(state.access.get(), fp_hex));
    } else if let Some(a) = access.as_mut() {
        let lost = a.poll();
        if lost != 0 {
            peer.withdraw(lost, inj_tx);
        }
    }
    if let Some(a) = access
        .as_ref()
        .filter(|a| a.expired(crate::clock::unix_secs()))
    {
        // Expiry ends the session as a decision, not a network drop. `quit_session`
        // clears `launch`; the host-side-ended arm then sends TERMINATION + disconnect
        // (GameStream has no AccessUpdate).
        let why = if a.revoked {
            "gamestream access record removed"
        } else {
            "gamestream access expired"
        };
        tracing::info!(reason = why, "gamestream: ending the session");
        state.quit_session(why);
        *access = None;
    }
}

/// The connected control peer and everything scoped to it. [`ControlPeer::reset`] ends it;
/// `host_seq` and `last_key` outlive it.
struct ControlPeer {
    id: Option<PeerID>,
    /// GCM nonce scheme, locked on the first packet that authenticates.
    scheme: Option<Scheme>,
    /// Consecutive decrypt failures; throttles the warn so a junk flood stays bounded.
    decrypt_fails: u64,
    /// What the client last heard over HDR-mode (0x010e). A client starts in SDR.
    hdr_signalled: Option<HdrMeta>,
    pads: SessionPads,
    /// SS_PEN/SS_TOUCH → tablet / wire touch. Clients send these only after seeing
    /// `SS_FF_PEN_TOUCH_EVENTS` (rtsp.rs).
    pointer: super::pen::GsPointer,
    /// The injector outlives the peer: whatever it still holds is released when it goes.
    held: crate::inject::held::HeldInput,
    /// Windows auto-repeat for the last held key; the tick injects what falls due.
    repeat: super::input::KeyRepeat,
    drops: GrantDrops,
    /// The client's last pad mask, so a withdrawn GAMEPAD grant rests exactly its pads.
    pads_active: u16,
    /// One host→client seq for every outbound message (rumble, HDR, termination). The GCM
    /// nonce is derived from it; a per-type counter would reuse (key, nonce) pairs.
    host_seq: u32,
    /// Last live GCM key. Ending a session clears `launch` (where the key lives), so without
    /// this copy the termination that must go out because it ended cannot seal.
    last_key: Option<[u8; 16]>,
}

impl ControlPeer {
    fn new() -> ControlPeer {
        ControlPeer {
            id: None,
            scheme: None,
            decrypt_fails: 0,
            hdr_signalled: None,
            pads: SessionPads::new(),
            pointer: super::pen::GsPointer::new(),
            held: Default::default(),
            repeat: super::input::KeyRepeat::for_this_host(),
            drops: GrantDrops::new(Plane::Gamestream),
            pads_active: 0,
            host_seq: 0,
            last_key: None,
        }
    }

    /// A grant withdrawn mid-session: let go of what it covered, or a key, button or stick
    /// stays down until the peer resets, as the native plane's `on_grants` does. Keys and
    /// pointer release through the injector; the pads rest in place on the mask the client
    /// last sent, since tearing a live pad is churn.
    fn withdraw(&mut self, lost: u32, inj_tx: &Sender<InputEvent>) {
        let keys = lost & GRANT_KEYBOARD != 0;
        for ev in self.held.release_classes(keys, lost & GRANT_POINTER != 0) {
            let _ = inj_tx.send(ev);
        }
        if keys {
            self.repeat = super::input::KeyRepeat::for_this_host();
        }
        if lost & GRANT_GAMEPAD == 0 {
            return;
        }
        for index in (0..MAX_PADS as i16).filter(|i| self.pads_active & (1 << i) != 0) {
            self.pads.handle(&GamepadEvent::State(GamepadFrame {
                index,
                active_mask: self.pads_active,
                buttons: 0,
                left_trigger: 0,
                right_trigger: 0,
                ls_x: 0,
                ls_y: 0,
                rs_x: 0,
                rs_y: 0,
            }));
        }
    }

    /// Inject the held key's repeat when it falls due, under the same grant as the key.
    fn pump_repeat(&mut self, inj_tx: &Sender<InputEvent>, grants: u32) {
        if let Some(ev) = self.repeat.due(std::time::Instant::now()) {
            if self.drops.permitted(grants, classify(ev.kind)) {
                let _ = inj_tx.send(ev);
            }
        }
    }

    fn next_seq(&mut self) -> u32 {
        let seq = self.host_seq;
        self.host_seq = seq.wrapping_add(1);
        seq
    }

    /// Forget the peer: drop its pads and tablet (destroying the uinput pen releases a held
    /// tool or tip kernel-side), release what the injector still holds, log the drop totals.
    fn reset(&mut self, inj_tx: &Sender<InputEvent>) {
        self.id = None;
        self.scheme = None;
        self.decrypt_fails = 0;
        self.hdr_signalled = None;
        self.pads = SessionPads::new();
        self.pads_active = 0;
        self.pointer = super::pen::GsPointer::new();
        self.repeat = super::input::KeyRepeat::for_this_host();
        for ev in self.held.release() {
            let _ = inj_tx.send(ev);
        }
        if let Some(totals) =
            std::mem::replace(&mut self.drops, GrantDrops::new(Plane::Gamestream)).summary()
        {
            tracing::info!(drops = %totals, "gamestream: access-grant drop totals for the session");
        }
    }

    /// TERMINATION, then `disconnect_later`, which flushes it first: a bare disconnect reads
    /// as `-1` on the client. Sealed only once the scheme is known; otherwise the disconnect
    /// goes alone rather than a packet the client cannot read. `false` with no peer.
    fn farewell<S: rusty_enet::Socket>(&mut self, host: &mut Host<S>) -> bool {
        let Some(pid) = self.id else {
            return false;
        };
        if let (Some(scheme), Some(key)) = (self.scheme, self.last_key) {
            let seq = self.next_seq();
            let wire = encrypt_control(&key, &scheme, seq, &termination_plaintext());
            if let Err(e) = host.peer_mut(pid).send(0, &Packet::reliable(&wire[..])) {
                tracing::warn!(error = ?e, "control: termination send failed");
            }
        }
        host.peer_mut(pid).disconnect_later(0);
        true
    }

    /// Pump force-feedback every tick (games block in EVIOCSFF until answered) and send the
    /// HDR-mode cue ahead of rumble. Legacy GCM nonces have no direction byte, so `host_seq`
    /// and the client's seq share (key, nonce) space; V2 separates them with `iv[10..12]`.
    /// Do not invent a per-type counter to "fix" it.
    fn pump_outbound<S: rusty_enet::Socket>(&mut self, host: &mut Host<S>, state: &AppState) {
        let (Some(pid), Some(scheme)) = (self.id, self.scheme) else {
            // No client/scheme yet: still answer FF uploads so games do not block.
            self.pads.pump_rumble(|_, _, _, _, _| {});
            return;
        };
        let key = state
            .launch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map(|s| s.gcm_key);
        let Some(key) = key else {
            return;
        };
        self.last_key = Some(key);
        let mut out: Vec<Vec<u8>> = Vec::new();
        // HDR-mode (0x010e / `IDX_HDR_MODE`) follows the frames the video thread encodes,
        // off again when they turn SDR. Stock Moonlight switches the TV only on this cue.
        let encoded_hdr = *state.gs.video_hdr.lock().unwrap_or_else(|e| e.into_inner());
        if encoded_hdr != self.hdr_signalled {
            let meta = encoded_hdr.or(self.hdr_signalled).unwrap_or_default();
            let pt = hdr_mode_plaintext(encoded_hdr.is_some(), &meta);
            let seq = self.next_seq();
            out.push(encrypt_control(&key, &scheme, seq, &pt));
            tracing::info!(
                on = encoded_hdr.is_some(),
                "control: signaled HDR mode to client (0x010e)"
            );
            self.hdr_signalled = encoded_hdr;
        }
        // Handle motors only. `0x010B` has no trigger-rumble id on this plane, and uinput
        // `FF_RUMBLE` has two fields anyway.
        let seq = &mut self.host_seq;
        self.pads.pump_rumble(|index, low, high, _lt, _rt| {
            let pt = super::gamepad::rumble_plaintext(index, low, high);
            out.push(encrypt_control(&key, &scheme, *seq, &pt));
            *seq = seq.wrapping_add(1);
        });
        for wire in out {
            if let Err(e) = host.peer_mut(pid).send(0, &Packet::reliable(&wire[..])) {
                tracing::warn!(error = ?e, "control send failed");
            }
        }
    }

    /// Open one encrypted control packet, locking the GCM scheme on the first that
    /// authenticates. A packet without the encrypted header is dropped before GCM and is not
    /// a decrypt failure; one no scheme opens counts, warned at 1, 2, 4, … so a junk flood
    /// stays bounded.
    fn open(&mut self, key: &[u8; 16], d: &[u8]) -> Option<Vec<u8>> {
        // Encrypted control packets begin with u16 LE encType = 0x0001 and an 8-byte header.
        if d.len() < 8 || d[0] != 0x01 || d[1] != 0x00 {
            return None;
        }
        match decrypt_control(key, d, &self.scheme) {
            Some((scheme, pt)) => {
                if self.scheme.is_none() {
                    tracing::info!(?scheme, "control: GCM scheme locked in");
                }
                self.scheme = Some(scheme);
                self.decrypt_fails = 0;
                Some(pt)
            }
            None => {
                self.decrypt_fails += 1;
                if self.decrypt_fails.is_power_of_two() {
                    tracing::warn!(
                        len = d.len(),
                        fails = self.decrypt_fails,
                        "control: GCM decrypt failed"
                    );
                }
                None
            }
        }
    }

    /// Inject one input plaintext under the grant mask, each class one mask test: gamepad to
    /// this session's pads, pen/touch through the pointer, keys and mouse to the injector.
    /// Decodable input lifts the suspend veto whether or not it is granted; a keepalive is
    /// not input and lifts nothing.
    fn inject(&mut self, state: &AppState, pt: &[u8], inj_tx: &Sender<InputEvent>, grants: u32) {
        // Gate gamepad before the manager sees it: without GAMEPAD the creating event never
        // arrives, so no uinput node and no pad-audio streamer.
        if let Some(gp) = super::gamepad::decode(pt) {
            crate::sleep_inhibit::note_input();
            if self.drops.permitted(grants, GrantClass::Gamepad) {
                state.counters.input_rich.fetch_add(1, Ordering::Relaxed);
                if let GamepadEvent::State(f) = &gp {
                    self.pads_active = f.active_mask;
                }
                self.pads.handle(&gp);
            }
            return;
        }
        if let Some(p) = super::input::decode_pointer(pt) {
            crate::sleep_inhibit::note_input();
            if self.drops.permitted(grants, GrantClass::Pointer) {
                state.counters.input_rich.fetch_add(1, Ordering::Relaxed);
                let held = &mut self.held;
                self.pointer.apply(&p, |ev| {
                    held.note(&ev);
                    let _ = inj_tx.send(ev);
                });
            }
            return;
        }
        if super::input::is_pointer_magic(pt) {
            warn_malformed_pointer(pt);
            return;
        }
        let events = super::input::decode(pt);
        if events.is_empty() {
            return; // keepalive / QoS / unhandled input kind
        }
        crate::sleep_inhibit::note_input();
        // One mask test, then the injector thread. A closed channel means the injector
        // died at startup; input is lossy, so drop silently.
        for ev in events {
            if self.drops.permitted(grants, classify(ev.kind)) {
                state.counters.input_events.fetch_add(1, Ordering::Relaxed);
                self.held.note(&ev);
                self.repeat.note(&ev, std::time::Instant::now());
                let _ = inj_tx.send(ev);
            }
        }
    }
}

/// One control packet: open it under the session key, then either a loss-recovery
/// request for the video thread or input under the grant mask. Nothing before `/launch`.
fn on_receive(
    state: &AppState,
    d: &[u8],
    peer: &mut ControlPeer,
    inj_tx: &Sender<InputEvent>,
    grants: u32,
) {
    let Some(key) = state
        .launch
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .map(|s| s.gcm_key)
    else {
        return;
    };
    let Some(pt) = peer.open(&key, d) else {
        return;
    };
    match ControlMsg::parse(&pt) {
        ControlMsg::Input => peer.inject(state, &pt, inj_tx, grants),
        msg => note_loss_recovery(state, msg),
    }
}

/// Apply a loss-recovery request to what the video thread drains: `rfi_range`,
/// `force_idr`, the loss counters.
fn note_loss_recovery(state: &AppState, msg: ControlMsg) {
    match msg {
        ControlMsg::InvalidateFrames(first, last) => {
            // Merged with a range the video thread has not drained yet: each lost frame
            // must stay invalid. Too wide a merge becomes a keyframe there.
            let mut slot = state.rfi_range.lock().unwrap_or_else(|e| e.into_inner());
            *slot = Some(match *slot {
                Some((pf, pl)) => (pf.min(first), pl.max(last)),
                None => (first, last),
            });
            drop(slot);
            tracing::debug!(first, last, "control: RFI request → invalidate ref frames");
        }
        ControlMsg::Keyframe(ty) => {
            state
                .force_idr
                .store(true, std::sync::atomic::Ordering::SeqCst);
            tracing::debug!(ty = %format_args!("{ty:#06x}"), "control: keyframe request");
        }
        ControlMsg::LossStats {
            lost,
            window_ms,
            last_good,
        } => {
            state
                .loss_stats
                .lost
                .fetch_add(lost, std::sync::atomic::Ordering::Relaxed);
            state
                .loss_stats
                .reports
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if lost > 0 {
                tracing::debug!(lost, window_ms, last_good, "control: client loss report");
            }
        }
        ControlMsg::FecLoss(lost) => {
            state
                .loss_stats
                .lost
                .fetch_add(lost, std::sync::atomic::Ordering::Relaxed);
            state
                .loss_stats
                .reports
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if lost > 0 {
                tracing::debug!(lost, "control: frame lost past FEC");
            }
        }
        ControlMsg::Ignored | ControlMsg::Input => {}
    }
}

/// Pointer magic whose body did not parse: a layout mismatch. The first few payloads go
/// out in hex so the log alone diagnoses it.
fn warn_malformed_pointer(pt: &[u8]) {
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

#[cfg(test)]
mod tests {
    use super::super::control_wire::tests::{fec, loss, msg, rfi};
    use super::super::control_wire::{seal_control, Aad, NonceKind};

    fn launched(peer_ip: Option<std::net::IpAddr>) -> Option<super::LaunchSession> {
        Some(super::LaunchSession {
            gcm_key: [0; 16],
            rikeyid: 0,
            width: 1920,
            height: 1080,
            fps: 60,
            appid: 1,
            host_audio: false,
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

    /// A flood of non-owner datagrams makes `receive` report none after the drop cap, so ENet's
    /// ACK and timeout pass runs; the datagrams behind the cap are read by the next call.
    #[test]
    fn a_non_owner_flood_does_not_hold_receive() {
        use rusty_enet::Socket;
        let inner = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        inner
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .unwrap();
        let to = inner.local_addr().unwrap();
        let state = super::super::test_state();
        let mut sock = super::OwnerFilteredSocket {
            inner,
            state: state.clone(),
        };
        let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let other: std::net::IpAddr = "10.9.9.9".parse().unwrap();
        *state.launch.lock().unwrap() = launched(Some(other));
        for _ in 0..super::MAX_DROPS_PER_RECEIVE + 5 {
            sender.send_to(&[1], to).unwrap();
        }
        let mut buf = [0u8; rusty_enet::MTU_MAX];
        assert!(sock.receive(&mut buf).unwrap().is_none());
        *state.launch.lock().unwrap() = launched(Some(sender.local_addr().unwrap().ip()));
        assert!(
            sock.receive(&mut buf).unwrap().is_some(),
            "the datagrams behind the cap stay queued for the owner"
        );
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
        assert_eq!(&pt[23..25], &50u16.to_le_bytes()); // minDisplayLuminance (1/10000 nit)
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

    /// No grants record at session start is ungoverned (full control, back-compat for
    /// existing pairings). A record governs; console edits fold in within one poll; "expire
    /// now" is a past deadline on the same watch. Deleting the record under a live session
    /// ends it: a deletion must never widen a session it was governing.
    #[test]
    fn session_access_resolves_folds_edits_and_expires() {
        use crate::native_pairing::{Access, NativePairing};
        use punktfunk_core::quic::{GRANT_ALL, GRANT_GAMEPAD};
        let p = crate::test_support::scratch().join("pf-gs-session-access.json");
        let np =
            std::sync::Arc::new(NativePairing::load_with(Some(p.clone()), None, false).unwrap());
        let now = crate::clock::unix_secs();

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

    /// Every way a peer ends — client disconnect, host-side end, last pairing removed —
    /// releases what it still holds and forgets its scheme, while the GCM seq runs on.
    #[test]
    fn a_reset_peer_releases_held_input_and_keeps_the_nonce_sequence() {
        use punktfunk_core::input::{InputEvent, InputKind};
        let mut peer = super::ControlPeer::new();
        peer.decrypt_fails = 3;
        peer.last_key = Some([7; 16]);
        assert_eq!(peer.next_seq(), 0);
        peer.held.note(&InputEvent {
            kind: InputKind::KeyDown,
            _pad: [0; 3],
            code: 30,
            x: 0,
            y: 0,
            flags: 0,
        });
        let (tx, rx) = std::sync::mpsc::channel();
        peer.reset(&tx);
        let ups: Vec<_> = rx.try_iter().map(|e| (e.kind, e.code)).collect();
        assert_eq!(ups, [(InputKind::KeyUp, 30)]);
        assert!(peer.held.is_empty());
        assert_eq!(peer.decrypt_fails, 0);
        assert!(peer.scheme.is_none() && peer.id.is_none());
        assert_eq!(peer.last_key, Some([7; 16]));
        assert_eq!(peer.next_seq(), 1);
    }

    use punktfunk_core::input::InputKind;
    use punktfunk_core::quic::{GRANT_ALL, GRANT_GAMEPAD, GRANT_KEYBOARD};
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::channel;

    const KEY: [u8; 16] = [0x42; 16];

    /// `NV_INPUT` keyboard packet: `size` BE, `magic` LE, flags, keyCode LE, modifiers.
    fn key_msg(down: bool, vk: u8) -> Vec<u8> {
        let magic: u32 = if down { 0x03 } else { 0x04 };
        let mut b = 12u32.to_be_bytes().to_vec();
        b.extend_from_slice(&magic.to_le_bytes());
        b.extend_from_slice(&[0, vk, 0, 0, 0, 0, 0, 0]);
        msg(0x0206, &b)
    }

    /// Gen5+ multi-controller frame: pad 0 active, one button down.
    fn gamepad_msg() -> Vec<u8> {
        let mut b = 30u32.to_be_bytes().to_vec();
        b.extend_from_slice(&0x0Cu32.to_le_bytes());
        let mut body = [0u8; 26];
        body[4] = 1; // active mask
        body[8] = 0x10; // buttons, low half
        b.extend_from_slice(&body);
        msg(0x0206, &b)
    }

    /// Seal as stock Moonlight does (`SS_ENC_CONTROL_V2`): forward key, 12-byte nonce with
    /// `seq` LE and `b"CC"`, tag before ciphertext, no AAD.
    fn client_seal(seq: u32, pt: &[u8]) -> Vec<u8> {
        seal_control(&KEY, &client_scheme(), seq, pt)
    }

    fn client_scheme() -> super::Scheme {
        super::Scheme {
            key_rev: false,
            nonce: NonceKind::V2 {
                seq_be: false,
                marker: *b"CC",
            },
            tag_first: true,
            aad: Aad::None,
        }
    }

    fn launched_state() -> std::sync::Arc<crate::host::AppState> {
        let state = super::super::test_state();
        *state.launch.lock().unwrap() =
            launched(None).map(|l| super::LaunchSession { gcm_key: KEY, ..l });
        state
    }

    /// Before `/launch` there is no key: nothing is tried, nothing counted.
    #[test]
    fn control_before_launch_is_ignored() {
        let state = super::super::test_state();
        let mut peer = super::ControlPeer::new();
        let (tx, rx) = channel();
        super::on_receive(
            &state,
            &client_seal(0, &key_msg(true, 30)),
            &mut peer,
            &tx,
            GRANT_ALL,
        );
        super::on_receive(&state, &[0xff; 40], &mut peer, &tx, GRANT_ALL);
        assert!(peer.scheme.is_none());
        assert_eq!(peer.decrypt_fails, 0);
        assert!(rx.try_iter().next().is_none());
    }

    /// A packet without the encrypted header is dropped before GCM and is not a decrypt
    /// failure; one with the header that no scheme opens counts as one and locks nothing.
    #[test]
    fn junk_never_locks_a_scheme_and_only_sealed_shapes_count_as_failures() {
        let state = launched_state();
        let mut peer = super::ControlPeer::new();
        let (tx, rx) = channel();
        for d in [
            &[][..],
            &[0x01, 0x00, 0x00][..],
            &[0x02, 0x00, 0, 0, 0, 0, 0, 0, 1, 2, 3][..],
        ] {
            super::on_receive(&state, d, &mut peer, &tx, GRANT_ALL);
        }
        assert_eq!(
            peer.decrypt_fails, 0,
            "header-rejected packets are not decrypt failures"
        );
        let mut junk = client_seal(1, &key_msg(true, 30));
        junk[20] ^= 0x80; // one tag bit
        super::on_receive(&state, &junk, &mut peer, &tx, GRANT_ALL);
        super::on_receive(
            &state,
            &[
                0x01, 0x00, 0, 0, 0, 0, 0, 0, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9,
            ],
            &mut peer,
            &tx,
            GRANT_ALL,
        );
        assert_eq!(peer.decrypt_fails, 2);
        assert!(peer.scheme.is_none());
        assert!(rx.try_iter().next().is_none());
    }

    /// The first packet that authenticates locks the scheme stock Moonlight uses; a later
    /// packet in any other construction is dropped and the lock holds.
    #[test]
    fn first_authenticating_packet_locks_the_client_scheme() {
        let state = launched_state();
        let mut peer = super::ControlPeer::new();
        peer.decrypt_fails = 5;
        let (tx, rx) = channel();
        super::on_receive(
            &state,
            &client_seal(7, &msg(0x0200, &[])),
            &mut peer,
            &tx,
            GRANT_ALL,
        );
        let s = peer.scheme.expect("locked");
        assert!(matches!(s.nonce, NonceKind::V2 { seq_be: false, marker } if &marker == b"CC"));
        assert!(s.tag_first && !s.key_rev && matches!(s.aad, Aad::None));
        assert_eq!(peer.decrypt_fails, 0);
        // Same key under the legacy low-byte nonce: a sweep would open it, the lock does not.
        let legacy = super::Scheme {
            nonce: NonceKind::LegacyLowByte,
            ..client_scheme()
        };
        super::on_receive(
            &state,
            &seal_control(&KEY, &legacy, 8, &key_msg(true, 30)),
            &mut peer,
            &tx,
            GRANT_ALL,
        );
        assert_eq!(peer.decrypt_fails, 1);
        assert!(matches!(peer.scheme.unwrap().nonce, NonceKind::V2 { .. }));
        assert!(rx.try_iter().next().is_none());
    }

    /// RFI with a range merges into what the video thread has not drained; without a usable
    /// range, and on 0x0302 / 0x0305, it is a keyframe. Loss reports add up per report.
    #[test]
    fn loss_recovery_requests_land_in_the_shared_state() {
        let state = launched_state();
        let mut peer = super::ControlPeer::new();
        let (tx, _rx) = channel();
        let mut seq = 0u32;
        let mut send = |pt: &[u8]| {
            super::on_receive(&state, &client_seal(seq, pt), &mut peer, &tx, GRANT_ALL);
            seq += 1;
        };
        send(&rfi(40, 47));
        assert_eq!(*state.rfi_range.lock().unwrap(), Some((40, 47)));
        send(&rfi(30, 35));
        assert_eq!(
            *state.rfi_range.lock().unwrap(),
            Some((30, 47)),
            "merged, not replaced"
        );
        assert!(!state.force_idr.load(Ordering::SeqCst));
        send(&rfi(9, 4));
        assert!(
            state.force_idr.load(Ordering::SeqCst),
            "no usable range → keyframe"
        );
        for ty in [0x0302u16, 0x0305] {
            state.force_idr.store(false, Ordering::SeqCst);
            send(&msg(ty, &[]));
            assert!(state.force_idr.load(Ordering::SeqCst), "{ty:#06x}");
        }
        send(&loss(3));
        send(&loss(0));
        send(&loss(-4)); // a negative count is none
        assert_eq!(state.loss_stats.lost.load(Ordering::Relaxed), 3);
        assert_eq!(state.loss_stats.reports.load(Ordering::Relaxed), 3);
        send(&fec(10, 6, 1));
        send(&fec(10, 8, 2));
        assert_eq!(state.loss_stats.lost.load(Ordering::Relaxed), 6);
        assert_eq!(state.loss_stats.reports.load(Ordering::Relaxed), 5);
        // A short 0x0201 and a 0x5502 that does not parse change nothing.
        send(&msg(0x0201, &[1, 0, 0, 0]));
        send(&msg(0x5502, &[0; 10]));
        assert_eq!(state.loss_stats.reports.load(Ordering::Relaxed), 5);
        assert_eq!(state.counters.input_events.load(Ordering::Relaxed), 0);
    }

    /// Every decodable input is one mask test: granted → injected and counted, otherwise
    /// dropped and counted per class.
    #[test]
    fn input_is_injected_only_under_its_grant() {
        let state = launched_state();
        let mut peer = super::ControlPeer::new();
        let (tx, rx) = channel();
        super::on_receive(
            &state,
            &client_seal(0, &key_msg(true, 30)),
            &mut peer,
            &tx,
            GRANT_KEYBOARD,
        );
        let got: Vec<_> = rx.try_iter().map(|e| (e.kind, e.code)).collect();
        assert_eq!(got, [(InputKind::KeyDown, 30)]);
        assert!(!peer.held.is_empty());
        assert_eq!(state.counters.input_events.load(Ordering::Relaxed), 1);
        super::on_receive(
            &state,
            &client_seal(1, &key_msg(false, 30)),
            &mut peer,
            &tx,
            0,
        );
        assert!(
            rx.try_iter().next().is_none(),
            "no KEYBOARD grant → nothing injected"
        );
        // The dropped key-up alone leaves the key held; the grant's withdrawal is what
        // releases it (`a_grant_withdrawn_mid_press_releases_what_it_held`).
        assert!(!peer.held.is_empty());
        super::on_receive(
            &state,
            &client_seal(2, &gamepad_msg()),
            &mut peer,
            &tx,
            GRANT_GAMEPAD,
        );
        assert_eq!(state.counters.input_rich.load(Ordering::Relaxed), 1);
        super::on_receive(&state, &client_seal(3, &gamepad_msg()), &mut peer, &tx, 0);
        assert_eq!(state.counters.input_rich.load(Ordering::Relaxed), 1);
        assert_eq!(
            peer.drops.summary().as_deref(),
            Some("Gamepad=1 Keyboard=1")
        );
        // A keepalive is not input: no event, no count, no drop.
        super::on_receive(
            &state,
            &client_seal(4, &msg(0x0200, &[])),
            &mut peer,
            &tx,
            0,
        );
        assert_eq!(
            peer.drops.summary().as_deref(),
            Some("Gamepad=1 Keyboard=1")
        );
    }

    /// A pointer magic whose body does not parse is a layout mismatch: logged, never injected,
    /// never a grant drop.
    #[test]
    fn a_pointer_packet_that_does_not_parse_is_dropped_whole() {
        let state = launched_state();
        let mut peer = super::ControlPeer::new();
        let (tx, rx) = channel();
        let mut b = 6u32.to_be_bytes().to_vec();
        b.extend_from_slice(&0x5500_0003u32.to_le_bytes()); // SS_PEN
        b.extend_from_slice(&[1, 0]);
        super::on_receive(
            &state,
            &client_seal(0, &msg(0x0206, &b)),
            &mut peer,
            &tx,
            GRANT_ALL,
        );
        assert!(rx.try_iter().next().is_none());
        assert_eq!(state.counters.input_rich.load(Ordering::Relaxed), 0);
        assert!(peer.drops.summary().is_none());
    }

    /// Arbitrary bytes, with and without the encrypted header, mutated from a real packet or
    /// not, before and after the scheme locks: no panic, nothing injected without a grant,
    /// and the lock never moves.
    #[test]
    fn random_control_packets_never_panic_or_inject() {
        let state = launched_state();
        let mut peer = super::ControlPeer::new();
        let (tx, rx) = channel();
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let valid = client_seal(1, &key_msg(true, 30));
        for round in 0..2 {
            for _ in 0..1500 {
                let mut d: Vec<u8> = (0..(next() % 80) as usize).map(|_| next() as u8).collect();
                match next() % 3 {
                    0 if d.len() >= 2 => {
                        d[0] = 1;
                        d[1] = 0;
                    }
                    1 => {
                        d = valid.clone();
                        let i = (next() as usize) % d.len();
                        d[i] ^= 1 << (next() % 8);
                    }
                    _ => {}
                }
                super::on_receive(&state, &d, &mut peer, &tx, 0);
                assert!(rx.try_iter().next().is_none());
            }
            if round == 0 {
                super::on_receive(&state, &valid, &mut peer, &tx, 0);
            }
            assert!(matches!(
                peer.scheme,
                Some(super::Scheme {
                    tag_first: true,
                    key_rev: false,
                    nonce: NonceKind::V2 { .. },
                    ..
                })
            ));
        }
    }

    /// A grant withdrawn mid-press lets go of what it covered: the key-up the client can no
    /// longer send goes out from here, the pads rest, and what the other grants hold stays.
    #[test]
    fn a_grant_withdrawn_mid_press_releases_what_it_held() {
        use crate::native_pairing::{Access, NativePairing};
        let owner = [0xab; 32];
        let fp_hex = hex::encode(owner);
        let p = crate::test_support::scratch().join("pf-gs-withdraw.json");
        let np =
            std::sync::Arc::new(NativePairing::load_with(Some(p.clone()), None, false).unwrap());
        np.add_with_access(
            "Deck",
            &fp_hex,
            Some(Access {
                grants: GRANT_ALL,
                expires_unix: None,
                until_disconnect: false,
            }),
        )
        .unwrap();
        let state = launched_state();
        *state.launch.lock().unwrap() = launched(None).map(|l| super::LaunchSession {
            gcm_key: KEY,
            owner_fp: Some(owner),
            ..l
        });
        let _ = state.access.set(np.clone());
        let mut access = None;
        let mut peer = super::ControlPeer::new();
        let (tx, rx) = channel();
        super::tick_access(&mut access, &state, &mut peer, &tx);
        let mask = access.as_ref().expect("resolved").mask;
        assert_eq!(mask, GRANT_ALL);
        super::on_receive(
            &state,
            &client_seal(0, &key_msg(true, 30)),
            &mut peer,
            &tx,
            mask,
        );
        super::on_receive(
            &state,
            &client_seal(1, &gamepad_msg()),
            &mut peer,
            &tx,
            mask,
        );
        assert_eq!(rx.try_iter().count(), 1, "the key-down went out");
        assert_eq!(peer.pads_active, 1);

        np.set_access(
            &fp_hex,
            Access {
                grants: GRANT_ALL & !GRANT_KEYBOARD,
                expires_unix: None,
                until_disconnect: false,
            },
        )
        .unwrap();
        super::tick_access(&mut access, &state, &mut peer, &tx);
        let ups: Vec<_> = rx.try_iter().map(|e| (e.kind, e.code)).collect();
        assert_eq!(ups, [(InputKind::KeyUp, 30)]);
        assert!(peer.held.is_empty());
        assert_eq!(
            peer.pads_active, 1,
            "the pads keep their mask: rested, not unplugged"
        );
        // The same edit again withdraws nothing more.
        super::tick_access(&mut access, &state, &mut peer, &tx);
        assert!(rx.try_iter().next().is_none());
        let _ = std::fs::remove_file(&p);
    }
}
