//! Embedder events → QUIC datagrams.
//!
//! Toward `HOST_CAP_GAMEPAD_STATE`, per-transition gamepad events fold into seq-stamped
//! `GamepadSnapshot`s. The datagram plane drops, reorders, and sheds oldest-first at
//! the 4 KiB send cap, so a lost edge would leave a held trigger stuck until the next
//! change. Snapshots heal on the next send; seq drops stale reorders; a 100 ms refresh
//! of every touched pad bounds loss to one interval (host rumble refresh is the same
//! idea at 500 ms). Keyboard/mouse/touch pass through. An older host keeps the legacy
//! per-transition gamepad events. `HOST_CAP_PAD_AUDIO` gates flags bits 8/9; without
//! it the whole flags word is the pad index.
//!
//! Events already queued behind the one just received fold into the same snapshot: an
//! embedder pushes one pad report as one event per axis, and a datagram per axis was six
//! per report on a stick being moved. The drain never waits, so nothing is delayed for it.
//!
//! A full controller-mouse pad ([`super::super::pad_mouse`]) bypasses the fold: its host snapshot
//! stays neutral and alive on the refresh, and its events become pointer, scroll and key events.
//! A touchpad-mode pad folds as usual except its touchpad click; its touchpads, like a full
//! pad's, arrive as contacts and move the pointer.
//!
//! Every datagram passes [`send_granted`]: what the live grants refuse never goes out. The host
//! drops the same classes; the gate runs after the controller-mouse fold, so those pads need
//! only the pointer grant.

use super::super::pad_mouse::{PadMouse, TICK};
use super::super::pad_touch::Contact;
use super::*;
use crate::input::gamepad::BTN_TOUCHPAD;
use crate::input::scroll::ScrollOutput;
use crate::input::{GamepadSnapshot, InputKind, PadMouseMode, MAX_PADS};

/// What the input task reads beside its queue.
pub(super) struct MouseArgs {
    /// Live grants (pointer and key outputs each need theirs), the stream mode pointer speed
    /// scales with, the invert-scroll toggle, the pad-mouse request and pad-audio caps.
    pub(super) client: Arc<ClientShared>,
    /// Host advertised `HOST_CAP2_SCROLL`: normalized events go out unchanged.
    pub(super) normalized_scroll: bool,
    /// The control stream's writer, toward a host that reads key edges there
    /// (`HOST_CAP2_INPUT_EDGES`). `None` keeps every event on the datagram plane.
    pub(super) edges: Option<tokio::sync::mpsc::Sender<CtrlRequest>>,
}

/// The final outbound gate for every ordinary input event — raw embedder sends
/// and controller-mouse output share it, so validation, the invert toggle, the
/// old-host `MouseScroll` conversion and the key lane each happen exactly once.
fn send_input(conn: &ClientConn, out: &mut ScrollOutput, args: &MouseArgs, ev: InputEvent) {
    let invert = args
        .client
        .scroll_invert
        .load(std::sync::atomic::Ordering::Relaxed);
    let Some(ev) = out.prepare(ev, invert) else {
        return;
    };
    if !granted(&args.client.access_grants, ev) {
        return;
    }
    // A key edge goes reliable when the host reads it there. A full control queue means
    // the control task is wedged; the datagram is then the better bet than a stall.
    if let Some(ctrl) = args.edges.as_ref().filter(|_| is_edge(ev.kind)) {
        if ctrl.try_send(CtrlRequest::InputEdge(ev)).is_ok() {
            return;
        }
    }
    let _ = conn.send_datagram(ev.encode().to_vec());
}

/// The kinds whose loss sticks: a release the network drops holds the key.
fn is_edge(kind: InputKind) -> bool {
    matches!(kind, InputKind::KeyDown | InputKind::KeyUp)
}

/// Whether the live grants cover `ev`'s [`crate::quic::classify`] class.
fn granted(grants: &AtomicU32, ev: InputEvent) -> bool {
    let grants = grants.load(std::sync::atomic::Ordering::Relaxed);
    grants & crate::quic::classify(ev.kind).bit() != 0
}

/// Send `ev` as a datagram when the live grants cover it.
fn send_granted(conn: &ClientConn, grants: &AtomicU32, ev: InputEvent) {
    if granted(grants, ev) {
        let _ = conn.send_datagram(ev.encode().to_vec());
    }
}

fn send_all(conn: &ClientConn, out: &mut ScrollOutput, args: &MouseArgs, evs: Vec<InputEvent>) {
    for ev in evs {
        send_input(conn, out, args, ev);
    }
}

/// Match the translator to the embedder's masks under the live grants. A pad changing mode
/// releases what it held first. A full-mouse pad's host snapshot goes neutral; a touchpad-mode
/// pad's loses only its touchpad click. No pointer grant clears every mode.
fn sync_mouse(
    conn: &ClientConn,
    mouse: &mut PadMouse,
    args: &MouseArgs,
    out: &mut ScrollOutput,
    pads: &mut [Option<GamepadSnapshot>; MAX_PADS],
    dirty: &mut [bool; MAX_PADS],
) {
    let grants = args
        .client
        .access_grants
        .load(std::sync::atomic::Ordering::Relaxed);
    if grants & crate::quic::GRANT_POINTER == 0 {
        args.client.pad_mouse.clear_all();
    }
    let full = args.client.pad_mouse.active(grants);
    let touchpad = args.client.pad_mouse.touchpad_active(grants);
    for idx in 0..MAX_PADS {
        let bit = 1u16 << idx;
        let want = if full & bit != 0 {
            PadMouseMode::Full
        } else if touchpad & bit != 0 {
            PadMouseMode::Touchpad
        } else {
            PadMouseMode::Off
        };
        let have = mouse.mode(idx);
        if want == have {
            continue;
        }
        if have != PadMouseMode::Off {
            send_all(conn, out, args, mouse.leave(idx));
        }
        if want == PadMouseMode::Off {
            continue;
        }
        let pad = idx as u8;
        mouse.enter(
            idx,
            want,
            pads[idx].unwrap_or(GamepadSnapshot {
                pad,
                ..Default::default()
            }),
        );
        if let Some(snap) = pads[idx].as_mut() {
            *snap = if want == PadMouseMode::Full {
                GamepadSnapshot {
                    pad,
                    seq: snap.seq,
                    ..Default::default()
                }
            } else {
                GamepadSnapshot {
                    buttons: snap.buttons & !BTN_TOUCHPAD,
                    ..*snap
                }
            };
            dirty[idx] = true;
        }
    }
}

/// One seq-stamped snapshot per pad flagged in `dirty`; clears the flags.
fn flush_dirty(
    conn: &ClientConn,
    grants: &AtomicU32,
    pads: &mut [Option<GamepadSnapshot>; MAX_PADS],
    seq: &mut [u8; MAX_PADS],
    dirty: &mut [bool; MAX_PADS],
) {
    for idx in 0..MAX_PADS {
        if !std::mem::take(&mut dirty[idx]) {
            continue;
        }
        if let Some(snap) = pads[idx].as_mut() {
            seq[idx] = seq[idx].wrapping_add(1);
            snap.seq = seq[idx];
            send_granted(conn, grants, snap.to_event());
        }
    }
}

pub(super) async fn run(
    conn: ClientConn,
    mut input_rx: tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
    mut pad_touch_rx: tokio::sync::mpsc::UnboundedReceiver<Contact>,
    gamepad_snapshots: bool,
    // HOST_CAP_PAD_AUDIO: only then do arrivals carry flags 8/9. An older host
    // reads the whole flags word as the pad index and would drop the kind.
    pad_audio: bool,
    mouse_args: MouseArgs,
) {
    use std::sync::atomic::Ordering;
    // bit0 haptics, bit1 speaker. Fed by [`NativeClient::set_pad_audio_caps`] and
    // by arrival events that already carry the bits.
    let pad_audio_caps = &mouse_args.client.pad_audio_caps;
    let mut mouse = PadMouse::default();
    // One seam for every outbound input event: Scroll stays whole toward a
    // normalized host, converts once to MouseScroll against an older one, and
    // the live invert flag applies to both plus controller-mouse output.
    let mut scroll_out = ScrollOutput::new(mouse_args.normalized_scroll);
    let mut mouse_tick = tokio::time::interval(TICK);
    mouse_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Unset while no stick is deflected, so a fresh push starts from one nominal tick.
    let mut last_mouse_tick: Option<std::time::Instant> = None;
    // Slot appears on the first event for that index; refresh never invents a pad.
    let mut pads: [Option<GamepadSnapshot>; MAX_PADS] = [None; MAX_PADS];
    // Pads folded into since their last send — see [`flush_dirty`].
    let mut dirty = [false; MAX_PADS];
    // Wrapping seq persists across remove/re-add on the same index. Removal takes
    // seq+1; a re-add continues, so the host does not reject a restarted-at-0 seq.
    let mut seq: [u8; MAX_PADS] = [0; MAX_PADS];
    // Removal re-sends still owed. A single lost removal strands a ghost pad; a few
    // time-spread seq-rising repeats, canceled the moment the pad is driven again.
    const REMOVE_RESENDS: u8 = 2;
    let mut remove_owed: [u8; MAX_PADS] = [0; MAX_PADS];
    // Declared kind + owed re-sends. The host needs the kind before the first
    // frame (mixed types); same lossy-plane burst as removal.
    const ARRIVAL_RESENDS: u8 = 2;
    let mut arrival: [Option<u8>; MAX_PADS] = [None; MAX_PADS];
    let mut arrival_owed: [u8; MAX_PADS] = [0; MAX_PADS];
    // Caps the last arrival actually carried. `set_pad_audio_caps` cannot reach this
    // task, so the tick re-arms the burst when the live registry moves.
    let mut arrival_caps_sent: [u8; MAX_PADS] = [0; MAX_PADS];
    let caps_now = |idx: usize| -> u8 {
        if pad_audio {
            pad_audio_caps[idx].load(Ordering::Relaxed)
        } else {
            0
        }
    };
    // Index plus bits 8/9 toward a PAD_AUDIO host; else byte-identical to the index.
    let arrival_flags = |idx: usize| -> u32 {
        let caps = caps_now(idx);
        crate::input::encode_gamepad_arrival(idx as u8, caps)
    };
    let mut refresh = tokio::time::interval(Duration::from_millis(100));
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            ev = input_rx.recv() => {
                let Some(first) = ev else { break };
                let mut pending = Some(first);
                while let Some(ev) = pending.take().or_else(|| input_rx.try_recv().ok()) {
                    let idx = ev.flags as usize;
                    if ev.kind == InputKind::GamepadButton
                        && ev.code == BTN_TOUCHPAD
                        && mouse.mode(idx) != PadMouseMode::Off
                    {
                        flush_dirty(&conn, &mouse_args.client.access_grants, &mut pads, &mut seq, &mut dirty);
                        let grants = mouse_args.client.access_grants.load(Ordering::Relaxed);
                        send_all(&conn, &mut scroll_out, &mouse_args, mouse.touchpad_click(idx, ev.x != 0, grants));
                        continue;
                    }
                    if matches!(ev.kind, InputKind::GamepadButton | InputKind::GamepadAxis)
                        && mouse.is_on(idx)
                    {
                        flush_dirty(&conn, &mouse_args.client.access_grants, &mut pads, &mut seq, &mut dirty);
                        let grants = mouse_args.client.access_grants.load(Ordering::Relaxed);
                        send_all(&conn, &mut scroll_out, &mouse_args, mouse.fold(idx, &ev, grants));
                        continue;
                    }
                    if ev.kind == InputKind::GamepadRemove && mouse.mode(idx) != PadMouseMode::Off {
                        send_all(&conn, &mut scroll_out, &mouse_args, mouse.leave(idx));
                        mouse_args.client.pad_mouse.clear(idx);
                    }
                    if gamepad_snapshots
                        && matches!(ev.kind, InputKind::GamepadButton | InputKind::GamepadAxis)
                        && idx < MAX_PADS
                    {
                        // Driven again: cancel owed removal (fresh snapshot seq already wins).
                        remove_owed[idx] = 0;
                        let snap = pads[idx].get_or_insert(GamepadSnapshot {
                            pad: idx as u8,
                            ..Default::default()
                        });
                        // Unknown axis: nothing to send (host legacy fold drops them too).
                        dirty[idx] |= snap.fold(&ev);
                        continue;
                    }
                    // Anything else goes out behind the snapshots folded so far.
                    flush_dirty(&conn, &mouse_args.client.access_grants, &mut pads, &mut seq, &mut dirty);
                    if gamepad_snapshots && ev.kind == InputKind::GamepadRemove && idx < MAX_PADS {
                        // Seq-stamped removal in the shared seq space so no reorder resurrects
                        // the pad. Arm the burst; drop owed arrival (a re-plug sends its own).
                        pads[idx] = None;
                        arrival[idx] = None;
                        arrival_owed[idx] = 0;
                        seq[idx] = seq[idx].wrapping_add(1);
                        remove_owed[idx] = REMOVE_RESENDS;
                        let rem = crate::input::InputEvent {
                            flags: crate::input::encode_gamepad_remove(idx as u8, seq[idx]),
                            ..ev
                        };
                        send_granted(&conn, &mouse_args.client.access_grants, rem);
                        continue;
                    }
                    if gamepad_snapshots && ev.kind == InputKind::GamepadArrival {
                        // Index is the low byte; bits 8/9 may carry caps (raw events). Fold
                        // them into the registry so the burst keeps them.
                        let (pad, ev_caps) = crate::input::decode_gamepad_arrival(ev.flags);
                        let idx = pad as usize;
                        if idx < MAX_PADS {
                            if ev_caps != 0 {
                                pad_audio_caps[idx].fetch_or(ev_caps, Ordering::Relaxed);
                            }
                            // Kind + burst so the host learns it before the first frame under loss.
                            arrival[idx] = Some(ev.code as u8);
                            arrival_owed[idx] = ARRIVAL_RESENDS;
                            arrival_caps_sent[idx] = caps_now(idx);
                            let arr = crate::input::InputEvent {
                                flags: arrival_flags(idx),
                                ..ev
                            };
                            send_granted(&conn, &mouse_args.client.access_grants, arr);
                            continue;
                        }
                    }
                    send_input(&conn, &mut scroll_out, &mouse_args, ev);
                }
                flush_dirty(&conn, &mouse_args.client.access_grants, &mut pads, &mut seq, &mut dirty);
                if !mouse.moving() {
                    last_mouse_tick = None;
                }
                let live = (0..MAX_PADS)
                    .filter(|&i| pads[i].is_some() || arrival[i].is_some())
                    .fold(0u16, |m, i| m | 1 << i);
                mouse_args.client.pad_mouse.set_live(live);
            }
            _ = mouse_args.client.pad_mouse.changed.notified() => {
                sync_mouse(&conn, &mut mouse, &mouse_args, &mut scroll_out, &mut pads, &mut dirty);
                flush_dirty(&conn, &mouse_args.client.access_grants, &mut pads, &mut seq, &mut dirty);
                if !mouse.moving() {
                    last_mouse_tick = None;
                }
            }
            Some(contact) = pad_touch_rx.recv() => {
                let height = mouse_args.client.mode.lock().map(|m| m.height).unwrap_or(0);
                let grants = mouse_args.client.access_grants.load(Ordering::Relaxed);
                send_all(&conn, &mut scroll_out, &mouse_args, mouse.contact(contact, height, grants));
            }
            _ = mouse_tick.tick(), if mouse.moving() => {
                let now = std::time::Instant::now();
                let dt = last_mouse_tick.map_or(TICK, |t| now.duration_since(t));
                last_mouse_tick = Some(now);
                let height = mouse_args.client.mode.lock().map(|m| m.height).unwrap_or(0);
                let grants = mouse_args.client.access_grants.load(Ordering::Relaxed);
                send_all(&conn, &mut scroll_out, &mouse_args, mouse.tick(dt.as_secs_f64(), height, grants));
            }
            _ = refresh.tick() => {
                // Grants arrive without a wake-up; losing the pointer grant ends mouse mode here.
                sync_mouse(&conn, &mut mouse, &mouse_args, &mut scroll_out, &mut pads, &mut dirty);
                for idx in 0..MAX_PADS {
                    // Caps moved after the burst drained: re-arm. Live declared pads only;
                    // a steady session sends nothing.
                    if arrival[idx].is_some()
                        && arrival_owed[idx] == 0
                        && caps_now(idx) != arrival_caps_sent[idx]
                    {
                        arrival_owed[idx] = ARRIVAL_RESENDS;
                    }
                    // Owed kind, even if the pad is still idle. Idempotent on the host.
                    if arrival_owed[idx] > 0 {
                        if let Some(kind) = arrival[idx] {
                            arrival_owed[idx] -= 1;
                            arrival_caps_sent[idx] = caps_now(idx);
                            let arr = crate::input::InputEvent {
                                kind: InputKind::GamepadArrival,
                                _pad: [0; 3],
                                code: kind as u32,
                                x: 0,
                                y: 0,
                                flags: arrival_flags(idx),
                            };
                            send_granted(&conn, &mouse_args.client.access_grants, arr);
                        } else {
                            arrival_owed[idx] = 0;
                        }
                    }
                    if pads[idx].is_some() {
                        dirty[idx] = true;
                    } else if remove_owed[idx] > 0 {
                        // Fresh-seq removal. Host no-op if already gone; a re-plug still wins by seq.
                        remove_owed[idx] -= 1;
                        seq[idx] = seq[idx].wrapping_add(1);
                        let rem = crate::input::InputEvent {
                            kind: InputKind::GamepadRemove,
                            _pad: [0; 3],
                            code: 0,
                            x: 0,
                            y: 0,
                            flags: crate::input::encode_gamepad_remove(idx as u8, seq[idx]),
                        };
                        send_granted(&conn, &mouse_args.client.access_grants, rem);
                    }
                }
                flush_dirty(&conn, &mouse_args.client.access_grants, &mut pads, &mut seq, &mut dirty);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{gamepad, InputEvent, InputKind};

    async fn loopback() -> (
        quinn::Endpoint,
        quinn::Endpoint,
        ClientConn,
        quinn::Connection,
    ) {
        let (server, client, host_conn, client_conn) = crate::quic::test_util::connect_pair().await;
        (server, client, ClientConn::new(client_conn), host_conn)
    }

    /// The input datagram inside a client datagram's kind.
    fn input_payload(dg: &[u8]) -> Vec<u8> {
        match crate::quic::v2::dgram::decode(dg) {
            Some(crate::quic::v2::dgram::Dgram::InputState(p)) => p.to_vec(),
            other => panic!("not an input datagram: {other:?}"),
        }
    }

    fn client(grants: u32) -> Arc<ClientShared> {
        let c = ClientShared::new(Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        });
        c.access_grants.store(grants, Ordering::Relaxed);
        Arc::new(c)
    }

    fn mouse_args(client: &Arc<ClientShared>) -> MouseArgs {
        MouseArgs {
            client: client.clone(),
            normalized_scroll: true,
            edges: None,
        }
    }

    fn key_up(vk: u32) -> InputEvent {
        InputEvent {
            kind: InputKind::KeyUp,
            ..key_down(vk)
        }
    }

    /// Toward a host that reads them, key edges leave on the control stream and nothing
    /// else does; a closed control lane falls back to the datagram.
    #[tokio::test]
    async fn key_edges_take_the_control_stream_when_the_host_reads_them() {
        let (_server, _client, client_conn, host_conn) = loopback().await;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (ctrl_tx, mut ctrl_rx) = tokio::sync::mpsc::channel(4);
        let client = client(crate::quic::GRANT_ALL);
        let args = MouseArgs {
            edges: Some(ctrl_tx),
            ..mouse_args(&client)
        };
        let task = tokio::spawn(run(client_conn, rx, no_touch(), true, false, args));

        tx.send(key_down(0x41)).unwrap();
        tx.send(key_up(0x41)).unwrap();
        tx.send(InputEvent {
            kind: InputKind::MouseMove,
            _pad: [0; 3],
            code: 0,
            x: 3,
            y: -2,
            flags: 0,
        })
        .unwrap();
        for want in [key_down(0x41), key_up(0x41)] {
            match ctrl_rx.recv().await {
                Some(CtrlRequest::InputEdge(ev)) => assert_eq!(ev, want),
                _ => panic!("a key edge on the control lane"),
            }
        }
        let motion = next_event(&host_conn, &[]).await;
        assert_eq!((motion.kind, motion.x), (InputKind::MouseMove, 3));
        assert!(
            tokio::time::timeout(Duration::from_millis(150), host_conn.read_datagram())
                .await
                .is_err(),
            "no key edge as a datagram"
        );

        drop(ctrl_rx);
        tx.send(key_down(0x42)).unwrap();
        let fallback = next_event(&host_conn, &[]).await;
        assert_eq!((fallback.kind, fallback.code), (InputKind::KeyDown, 0x42));
        task.abort();
    }

    /// A touch queue nobody feeds.
    fn no_touch() -> tokio::sync::mpsc::UnboundedReceiver<Contact> {
        tokio::sync::mpsc::unbounded_channel().1
    }

    fn lead(pad: u8, x: f64) -> Contact {
        Contact {
            pad,
            surface: 0,
            finger: 0,
            touch: true,
            click: None,
            x,
            y: 0.0,
            raw: false,
        }
    }

    fn key_down(vk: u32) -> InputEvent {
        InputEvent {
            kind: InputKind::KeyDown,
            _pad: [0; 3],
            code: vk,
            x: 0,
            y: 0,
            flags: 0,
        }
    }

    fn button(bit: u32, pad: u32) -> InputEvent {
        InputEvent {
            kind: InputKind::GamepadButton,
            _pad: [0; 3],
            code: bit,
            x: 1,
            y: 0,
            flags: pad,
        }
    }

    /// Next datagram that is not a snapshot of a pad in `skip`. Pad 0 is the mouse pad.
    async fn next_event(host: &quinn::Connection, skip: &[u8]) -> InputEvent {
        loop {
            let dg = tokio::time::timeout(Duration::from_secs(2), host.read_datagram())
                .await
                .expect("a datagram")
                .unwrap();
            let ev = InputEvent::decode(&input_payload(&dg)).unwrap();
            match GamepadSnapshot::from_event(&ev) {
                Some(s) if skip.contains(&s.pad) => {
                    assert!(s.pad != 0 || s.buttons == 0, "a mouse pad stays neutral")
                }
                _ => return ev,
            }
        }
    }

    /// Entering sends the pad neutral; its presses become keys and its touchpad moves the pointer
    /// while pad 1 still forwards.
    #[tokio::test]
    async fn a_mouse_pad_goes_neutral_and_its_buttons_become_keys() {
        let (_server, _client, client_conn, host_conn) = loopback().await;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (touch_tx, touch_rx) = tokio::sync::mpsc::unbounded_channel();
        let client = client(crate::quic::GRANT_ALL);
        let task = tokio::spawn(run(
            client_conn,
            rx,
            touch_rx,
            true,
            false,
            mouse_args(&client),
        ));

        tx.send(button(gamepad::BTN_A, 0)).unwrap();
        let held = next_event(&host_conn, &[]).await;
        assert_eq!(
            GamepadSnapshot::from_event(&held).unwrap().buttons,
            gamepad::BTN_A
        );

        client.pad_mouse.request(1);
        // A refresh of the held A may still be in flight; the neutral snapshot follows it.
        loop {
            let snap = GamepadSnapshot::from_event(&next_event(&host_conn, &[]).await)
                .expect("only snapshots so far");
            if snap.buttons == 0 {
                assert_eq!(snap.pad, 0);
                break;
            }
        }

        tx.send(button(gamepad::BTN_B, 0)).unwrap();
        let esc = next_event(&host_conn, &[0]).await;
        assert_eq!((esc.kind, esc.code), (InputKind::KeyDown, 0x1B));

        for x in [0.0, 1.0 / 3.0] {
            touch_tx.send(lead(0, x)).unwrap();
        }
        let moved = next_event(&host_conn, &[0]).await;
        assert_eq!(moved.kind, InputKind::MouseMove);
        assert!((539..=540).contains(&moved.x), "{}", moved.x);

        tx.send(button(gamepad::BTN_A, 1)).unwrap();
        let other = next_event(&host_conn, &[0]).await;
        let other = GamepadSnapshot::from_event(&other).expect("pad 1 forwards");
        assert_eq!((other.pad, other.buttons), (1, gamepad::BTN_A));
        assert_eq!(
            client.pad_mouse.live(),
            0b11,
            "both pads are live on the host"
        );

        client.pad_mouse.request(0);
        let up = next_event(&host_conn, &[0, 1]).await;
        assert_eq!(
            (up.kind, up.code),
            (InputKind::KeyUp, 0x1B),
            "leaving releases Escape"
        );
        task.abort();
    }

    /// A touchpad-mode pad keeps playing: its buttons reach the host snapshot, its touchpad
    /// click and touch become the mouse.
    #[tokio::test]
    async fn a_touchpad_mode_pad_plays_while_its_touchpad_clicks() {
        let (_server, _client, client_conn, host_conn) = loopback().await;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (touch_tx, touch_rx) = tokio::sync::mpsc::unbounded_channel();
        let client = client(crate::quic::GRANT_ALL);
        client.pad_mouse.set_mode(1, PadMouseMode::Touchpad);
        let task = tokio::spawn(run(
            client_conn,
            rx,
            touch_rx,
            true,
            false,
            mouse_args(&client),
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;

        tx.send(button(gamepad::BTN_A, 0)).unwrap();
        let snap = GamepadSnapshot::from_event(&next_event(&host_conn, &[]).await).unwrap();
        assert_eq!(snap.buttons, gamepad::BTN_A, "the game still gets A");

        tx.send(button(BTN_TOUCHPAD, 0)).unwrap();
        let click = next_event(&host_conn, &[0]).await;
        assert_eq!((click.kind, click.code), (InputKind::MouseButtonDown, 1));
        for x in [0.0, 0.5] {
            touch_tx.send(lead(0, x)).unwrap();
        }
        assert_eq!(
            next_event(&host_conn, &[0]).await.kind,
            InputKind::MouseMove
        );
        task.abort();
    }

    /// A controller-only session sends its pad and never the key queued ahead of it.
    #[tokio::test]
    async fn a_key_without_the_keyboard_grant_stays_off_the_wire() {
        let (_server, _client, client_conn, host_conn) = loopback().await;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let client = client(crate::quic::GRANT_GAMEPAD);
        let task = tokio::spawn(run(
            client_conn,
            rx,
            no_touch(),
            true,
            false,
            mouse_args(&client),
        ));

        tx.send(key_down(0x41)).unwrap();
        tx.send(button(gamepad::BTN_A, 0)).unwrap();
        let first = next_event(&host_conn, &[]).await;
        let snap = GamepadSnapshot::from_event(&first).expect("the pad, not the key");
        assert_eq!((snap.pad, snap.buttons), (0, gamepad::BTN_A));
        task.abort();
    }

    /// Without the gamepad grant a pad sends nothing, but a controller-mouse pad still
    /// drives keys: the gate runs after the fold.
    #[tokio::test]
    async fn a_pad_without_the_gamepad_grant_only_reaches_the_host_as_a_mouse() {
        let (_server, _client, client_conn, host_conn) = loopback().await;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let client = client(crate::quic::GRANT_POINTER | crate::quic::GRANT_KEYBOARD);
        client.pad_mouse.request(1);
        let task = tokio::spawn(run(
            client_conn,
            rx,
            no_touch(),
            true,
            false,
            mouse_args(&client),
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;

        tx.send(button(gamepad::BTN_B, 0)).unwrap();
        tx.send(button(gamepad::BTN_A, 1)).unwrap();
        tx.send(key_down(0x41)).unwrap();
        for want in [0x1B, 0x41] {
            let ev = next_event(&host_conn, &[]).await;
            assert_eq!(
                (ev.kind, ev.code),
                (InputKind::KeyDown, want),
                "no pad snapshot"
            );
        }
        task.abort();
    }

    /// Six axis events queued together — one pad report — leave as ONE snapshot carrying all six,
    /// not six datagrams each carrying one more axis. Current-thread runtime, so the task cannot
    /// run between the sends.
    #[tokio::test]
    async fn one_pad_report_leaves_as_one_snapshot() {
        let (_server, _client, client_conn, host_conn) = loopback().await;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let client = client(crate::quic::GRANT_ALL);
        let task = tokio::spawn(run(
            client_conn,
            rx,
            no_touch(),
            true,
            false,
            mouse_args(&client),
        ));
        let axes = [
            (gamepad::AXIS_LS_X, 1_000),
            (gamepad::AXIS_LS_Y, -2_000),
            (gamepad::AXIS_RS_X, 3_000),
            (gamepad::AXIS_RS_Y, -4_000),
            (gamepad::AXIS_LT, 50),
            (gamepad::AXIS_RT, 200),
        ];
        for (code, x) in axes {
            tx.send(InputEvent {
                kind: InputKind::GamepadAxis,
                _pad: [0; 3],
                code,
                x,
                y: 0,
                flags: 0,
            })
            .unwrap();
        }
        let dg = tokio::time::timeout(Duration::from_secs(2), host_conn.read_datagram())
            .await
            .expect("a datagram")
            .unwrap();
        let snap = GamepadSnapshot::from_event(&InputEvent::decode(&input_payload(&dg)).unwrap())
            .expect("a snapshot");
        assert_eq!(
            (
                snap.seq,
                snap.ls_x,
                snap.ls_y,
                snap.rs_x,
                snap.rs_y,
                snap.left_trigger,
                snap.right_trigger
            ),
            (1, 1_000, -2_000, 3_000, -4_000, 50, 200),
            "the first datagram is the whole report under seq 1"
        );
        task.abort();
    }
}
