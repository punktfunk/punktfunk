//! The AU-ready wake on the real tick loop (`capture_tick` → `encode_and_send` →
//! `sleep_to_next`), in real time: a feed whose frames arrive at the stream rate, and a fake
//! encoder whose "GPU" finishes each encode `E` after it starts, two slots deep. Each run
//! measures how long an AU waits between its encode finishing and its hand-off to the send
//! thread, with a backend that rings the wake and with one that does not.

use super::super::send::{SendMsg, SendThread};
use super::super::swap::tests::fake;
use crate::capture::CapturedFrame;
use pf_capture::Paced;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

fn frame() -> CapturedFrame {
    CapturedFrame {
        width: 4,
        height: 4,
        pts_ns: 0,
        format: crate::capture::PixelFormat::Bgrx,
        payload: pf_frame::FramePayload::Cpu(vec![0; 64]),
        cursor: None,
        provenance: Default::default(),
    }
}

/// The mailbox, the AU nudge, and every wait the loop asked of it: gate, deadline, answer.
#[derive(Default)]
struct FeedState {
    frame: Option<CapturedFrame>,
    /// The loop took a frame its next submit holds a buffer for.
    took: bool,
    encoded: bool,
    waits: Vec<(Instant, Instant, Paced)>,
}

type Shared<T> = Arc<(Mutex<T>, Condvar)>;

/// A one-deep mailbox a producer thread fills every `period`, waited the way the Linux
/// feeds wait: nothing but a nudge before `not_before`, then a frame or the deadline.
struct Feed {
    st: Shared<FeedState>,
    stop: Arc<AtomicBool>,
    producer: Option<std::thread::JoinHandle<()>>,
}

impl Feed {
    fn start(period: Duration) -> Feed {
        let st: Shared<FeedState> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (s, quit) = (st.clone(), stop.clone());
        let producer = std::thread::spawn(move || {
            let mut at = Instant::now();
            while !quit.load(Ordering::Relaxed) {
                at += period;
                std::thread::sleep(at.saturating_duration_since(Instant::now()));
                s.0.lock().unwrap().frame = Some(frame());
                s.1.notify_all();
            }
        });
        Feed {
            st,
            stop,
            producer: Some(producer),
        }
    }
}

impl Feed {
    /// A source that needs a buffer back before its next frame, as a screencast pool does:
    /// each frame takes one from `free` and lands a compositor phase later, the phases in
    /// turn. The encoder returns the buffer when the frame's access unit is collected.
    fn start_on_return(free: Shared<usize>, phases: Vec<Duration>) -> Feed {
        let st: Shared<FeedState> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (s, quit) = (st.clone(), stop.clone());
        let producer = std::thread::spawn(move || {
            for phase in phases.iter().cycle() {
                let (lock, cv) = &*free;
                let mut n = lock.lock().unwrap();
                while *n == 0 {
                    if quit.load(Ordering::Relaxed) {
                        return;
                    }
                    n = cv.wait_timeout(n, Duration::from_millis(2)).unwrap().0;
                }
                *n -= 1;
                drop(n);
                std::thread::sleep(*phase);
                // A frame the loop never took gives its buffer back.
                if s.0.lock().unwrap().frame.replace(frame()).is_some() {
                    *lock.lock().unwrap() += 1;
                }
                s.1.notify_all();
                if quit.load(Ordering::Relaxed) {
                    return;
                }
            }
        });
        Feed {
            st,
            stop,
            producer: Some(producer),
        }
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(p) = self.producer.take() {
            let _ = p.join();
        }
    }
}

impl pf_capture::Feed for Feed {
    fn wait_arrival(&mut self, deadline: Instant) {
        self.wait_paced(Instant::now(), deadline);
    }

    fn wait_paced(&mut self, not_before: Instant, deadline: Instant) -> Paced {
        let (lock, cv) = &*self.st;
        let mut s = lock.lock().unwrap();
        let answer = loop {
            if std::mem::take(&mut s.encoded) {
                break Paced::Encoded;
            }
            let now = Instant::now();
            if now >= not_before && (s.frame.is_some() || now >= deadline) {
                break Paced::Due;
            }
            let until = if now < not_before {
                not_before
            } else {
                deadline
            };
            s = cv.wait_timeout(s, until - now).unwrap().0;
        };
        s.waits.push((not_before, deadline, answer));
        answer
    }

    fn au_wake(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let st = self.st.clone();
        Some(Arc::new(move || {
            st.0.lock().unwrap().encoded = true;
            st.1.notify_all();
        }))
    }
}

impl crate::capture::Capturer for Feed {
    fn next_frame(&mut self, _budget: pf_capture::FirstFrame) -> anyhow::Result<CapturedFrame> {
        anyhow::bail!("the test feed only paces")
    }

    fn try_latest(&mut self) -> anyhow::Result<Option<CapturedFrame>> {
        let mut s = self.st.0.lock().unwrap();
        let frame = s.frame.take();
        s.took |= frame.is_some();
        Ok(frame)
    }

    fn feed(&mut self) -> Option<&mut dyn pf_capture::Feed> {
        Some(self)
    }
}

/// The fake GPU: encodes in flight `(seq, due)`, finished ones not yet polled, and when each
/// finished.
#[derive(Default)]
struct Gpu {
    running: VecDeque<(u32, Instant)>,
    done: VecDeque<u32>,
    finished: Vec<(u32, Instant)>,
    stop: bool,
}

/// Encodes serially, `encode` each, two slots deep. Pipelined, `poll` only takes what is
/// done and, when `rings`, the GPU thread rings the loop's wake as the encode finishes, as
/// the NVENC retrieve thread and the Vulkan fence waiter do. Sync, `poll` waits.
struct Pipelined {
    gpu: Shared<Gpu>,
    wake: crate::encode::AuWake,
    thread: Option<std::thread::JoinHandle<()>>,
    encode: Duration,
    pipelined: bool,
    seq: u32,
    submits: Arc<AtomicU32>,
    /// The source's free buffers and its mailbox, when it waits for buffers
    /// ([`Feed::start_on_return`]), and whether each submit not yet polled holds one.
    pool: Option<(Shared<usize>, Shared<FeedState>)>,
    holds: VecDeque<bool>,
    /// Repeats submitted while an earlier encode was still running.
    busy_repeats: Arc<AtomicU32>,
}

impl Pipelined {
    fn new(encode: Duration, pipelined: bool, rings: bool) -> Pipelined {
        let gpu: Shared<Gpu> = Arc::default();
        let wake = crate::encode::AuWake::default();
        let (g, w) = (gpu.clone(), wake.clone());
        let thread = std::thread::spawn(move || {
            let (lock, cv) = &*g;
            let mut s = lock.lock().unwrap();
            while !s.stop {
                let Some(&(seq, due)) = s.running.front() else {
                    s = cv.wait(s).unwrap();
                    continue;
                };
                let now = Instant::now();
                if now < due {
                    s = cv.wait_timeout(s, due - now).unwrap().0;
                    continue;
                }
                s.running.pop_front();
                s.done.push_back(seq);
                s.finished.push((seq, now));
                cv.notify_all();
                if pipelined && rings {
                    drop(s);
                    w.ring();
                    s = lock.lock().unwrap();
                }
            }
        });
        Pipelined {
            gpu,
            wake,
            thread: Some(thread),
            encode,
            pipelined,
            seq: 0,
            submits: Arc::default(),
            pool: None,
            holds: VecDeque::new(),
            busy_repeats: Arc::default(),
        }
    }
}

impl Drop for Pipelined {
    fn drop(&mut self) {
        self.gpu.0.lock().unwrap().stop = true;
        self.gpu.1.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl crate::encode::Encoder for Pipelined {
    fn submit(&mut self, _frame: &CapturedFrame) -> anyhow::Result<()> {
        self.submits.fetch_add(1, Ordering::Relaxed);
        if let Some((_, feed)) = &self.pool {
            let fresh = std::mem::take(&mut feed.0.lock().unwrap().took);
            self.holds.push_back(fresh);
            if !fresh && !self.gpu.0.lock().unwrap().running.is_empty() {
                self.busy_repeats.fetch_add(1, Ordering::Relaxed);
            }
        }
        let (lock, cv) = &*self.gpu;
        let mut s = lock.lock().unwrap();
        while s.running.len() >= 2 {
            s = cv.wait(s).unwrap();
        }
        let start = s.running.back().map_or(Instant::now(), |&(_, due)| due);
        s.running
            .push_back((self.seq, start.max(Instant::now()) + self.encode));
        self.seq += 1;
        cv.notify_all();
        Ok(())
    }

    fn poll(&mut self) -> anyhow::Result<Option<crate::encode::EncodedFrame>> {
        let (lock, cv) = &*self.gpu;
        let mut s = lock.lock().unwrap();
        while !self.pipelined && s.done.is_empty() && !s.running.is_empty() {
            s = cv.wait(s).unwrap();
        }
        let done = s.done.pop_front();
        drop(s);
        if done.is_some() && self.holds.pop_front() == Some(true) {
            if let Some((free, _)) = &self.pool {
                *free.0.lock().unwrap() += 1;
                free.1.notify_all();
            }
        }
        Ok(done.map(|seq| crate::encode::EncodedFrame {
            data: seq.to_le_bytes().to_vec(),
            pts_ns: 0,
            keyframe: false,
            recovery_anchor: false,
            recovery_point: false,
            recovery_close: false,
            chunk_aligned: false,
        }))
    }

    fn set_au_wake(&mut self, wake: Option<Arc<dyn Fn() + Send + Sync>>) {
        self.wake.set(wake);
    }

    fn flush(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// One run's outcome.
struct Run {
    /// Encode finished → handed to the send thread, ms, past the warm-up.
    ages: Vec<f64>,
    ticks: u32,
    submits: u32,
    fresh: u64,
    repeats: u64,
    /// Waits the encoder's wake ended.
    encoded: usize,
    /// Ticks whose waits, resumed after AU-ready wakes, asked more than one gate, or more
    /// deadlines than the two a tick has: the keep-alive and the one held for an owed AU.
    moved_gates: usize,
    /// Repeats the encoder was handed while still encoding ([`Feed::start_on_return`] runs).
    busy_repeats: u32,
}

/// The loop for `run_for` at `hz`, each encode taking `encode`, fed at the stream rate.
fn run(hz: u32, encode: Duration, pipelined: bool, rings: bool, run_for: Duration) -> Run {
    run_fed(hz, encode, pipelined, rings, run_for, None)
}

/// [`run`], or with `phases` a source that waits for a buffer back
/// ([`Feed::start_on_return`]) out of a pool of two: the encoder's slots hold them all.
fn run_fed(
    hz: u32,
    encode: Duration,
    pipelined: bool,
    rings: bool,
    run_for: Duration,
    phases: Option<&[Duration]>,
) -> Run {
    let period = Duration::from_secs(1) / hz;
    let mut f = fake();
    let (tx, rx) = std::sync::mpsc::sync_channel::<SendMsg>(256);
    let handed: Arc<Mutex<Vec<(u32, Instant)>>> = Arc::default();
    let h = handed.clone();
    let consumer = std::thread::spawn(move || {
        while let Ok(msg) = rx.recv() {
            if let SendMsg::Frame(m) = msg {
                let seq = u32::from_le_bytes(m.data[..4].try_into().unwrap());
                h.lock().unwrap().push((seq, Instant::now()));
            }
        }
    });
    f.st.send = SendThread::new(Arc::new(tx), consumer);
    let mut enc = Pipelined::new(encode, pipelined, rings);
    let (gpu, submits) = (enc.gpu.clone(), enc.submits.clone());
    let busy_repeats = enc.busy_repeats.clone();
    let feed = match phases {
        None => Feed::start(period),
        Some(phases) => {
            let free: Shared<usize> = Arc::new((Mutex::new(2), Condvar::new()));
            let feed = Feed::start_on_return(free.clone(), phases.to_vec());
            enc.pool = Some((free, feed.st.clone()));
            feed
        }
    };
    let feed_st = feed.st.clone();
    f.st.live.capturer = Box::new(feed);
    f.st.live.enc = Box::new(enc);
    f.st.live.interval = period;
    f.st.src_period_ns = Some(period.as_nanos() as u64);
    f.st.clock.inflight.clear();
    f.st.next = Instant::now();
    f.st.publish_arrival_poke();
    let end = Instant::now() + run_for;
    let mut ticks = 0;
    while Instant::now() < end {
        let tick = f.st.capture_tick().unwrap().expect("the session runs");
        let (t_cap, repeat) = (tick.t_cap, tick.repeat);
        ticks += 1;
        match f.st.encode_and_send(tick).unwrap() {
            super::Flow::Next => {}
            _ => panic!("the tick ended the session"),
        }
        match f.st.sleep_to_next(t_cap, repeat).unwrap() {
            super::Flow::Next => {}
            _ => panic!("the wait ended the session"),
        }
    }
    let (fresh, repeats) = (f.st.diag.diag_new, f.st.diag.diag_repeat);
    drop(f);
    let finished = gpu.0.lock().unwrap().finished.clone();
    let handed = handed.lock().unwrap().clone();
    let ages = finished
        .iter()
        .skip(5)
        .filter_map(|&(seq, at)| {
            let (_, out) = handed.iter().find(|&&(s, _)| s == seq)?;
            Some(out.saturating_duration_since(at).as_secs_f64() * 1e3)
        })
        .collect();
    let waits = &feed_st.0.lock().unwrap().waits;
    let encoded = waits.iter().filter(|w| w.2 == Paced::Encoded).count();
    // One tick's waits: each but the last ended on an AU-ready wake.
    let moved_gates = waits
        .chunk_by(|a, _| a.2 == Paced::Encoded)
        .filter(|tick| {
            let gates: std::collections::BTreeSet<_> = tick.iter().map(|w| w.0).collect();
            let deadlines: std::collections::BTreeSet<_> = tick.iter().map(|w| w.1).collect();
            gates.len() > 1 || deadlines.len() > 2
        })
        .count();
    Run {
        ages,
        ticks,
        submits: submits.load(Ordering::Relaxed),
        fresh,
        repeats,
        encoded,
        moved_gates,
        busy_repeats: busy_repeats.load(Ordering::Relaxed),
    }
}

fn pct(v: &[f64], q: f64) -> f64 {
    let mut v = v.to_vec();
    v.sort_by(f64::total_cmp);
    v.get(((v.len() as f64 * q) as usize).min(v.len().saturating_sub(1)))
        .copied()
        .unwrap_or(f64::NAN)
}

/// Escalated (pipelined) at 60 and 120 Hz with 3, 6 and 10 ms encodes: a backend that rings
/// hands each AU off at once instead of after the next frame's submit, with no encode the
/// wake caused, the same fresh frames and repeats, and every resumed wait on the gate and
/// deadline it left. `--nocapture` prints the before/after table.
#[test]
fn an_au_ready_wake_hands_a_pipelined_au_off_at_once() {
    let run_for = Duration::from_millis(600);
    for hz in [60u32, 120] {
        for encode_ms in [3u64, 6, 10] {
            let encode = Duration::from_millis(encode_ms);
            let today = run(hz, encode, true, false, run_for);
            let woken = run(hz, encode, true, true, run_for);
            for (name, r) in [("today", &today), ("au wake", &woken)] {
                println!(
                    "{hz} Hz, encode {encode_ms} ms, {name}: AU age mean {:.2} p50 {:.2} p90 \
                     {:.2} max {:.2} ms over {} AUs; ticks {} submits {} fresh {} repeats {} \
                     AU wakes {}",
                    mean(&r.ages),
                    pct(&r.ages, 0.5),
                    pct(&r.ages, 0.9),
                    pct(&r.ages, 1.0),
                    r.ages.len(),
                    r.ticks,
                    r.submits,
                    r.fresh,
                    r.repeats,
                    r.encoded
                );
                assert_eq!(
                    r.submits, r.ticks,
                    "{hz}/{encode_ms} {name}: one submit a tick"
                );
                assert_eq!(
                    r.moved_gates, 0,
                    "{hz}/{encode_ms} {name}: a resumed wait moved"
                );
            }
            assert_eq!(today.encoded, 0, "{hz}/{encode_ms}: no wake without a ring");
            assert!(woken.encoded > 0, "{hz}/{encode_ms}: the wake ended waits");
            // A loaded box runs a different number of ticks each time: compare the ticks
            // that had no fresh frame, not the totals.
            let stale = |r: &Run| u64::from(r.ticks).saturating_sub(r.fresh);
            assert!(
                stale(&today).abs_diff(stale(&woken)) <= 3,
                "{hz}/{encode_ms}: fresh {} of {} ticks vs {} of {}",
                today.fresh,
                today.ticks,
                woken.fresh,
                woken.ticks
            );
            let repeats = today.repeats.abs_diff(woken.repeats);
            assert!(repeats <= 2, "{hz}/{encode_ms}: repeats moved");
            let after = pct(&woken.ages, 0.5);
            assert!(
                after < 1.5,
                "{hz}/{encode_ms}: AU age p50 {after:.2} ms with the wake"
            );
            // An encode past the period backs the two slots up: the loop then waits on the
            // GPU in its submit, and today's AU is as fresh as it gets.
            let (before, after) = (mean(&today.ages), mean(&woken.ages));
            let bound = if encode < Duration::from_secs(1) / hz {
                before
            } else {
                before + 0.25
            };
            assert!(
                after < bound,
                "{hz}/{encode_ms}: mean {after:.2} ms, today {before:.2} ms"
            );
        }
    }
}

fn mean(v: &[f64]) -> f64 {
    v.iter().sum::<f64>() / v.len() as f64
}

/// A screencast source behind a saturated encoder: 120 Hz, 12 ms encodes, the next frame only
/// once a buffer is back and then a compositor phase later, every fourth one late. A late
/// frame finds the encoder still on the one before it. No repeat goes out ahead of an access
/// unit the encoder still owes, so the loop that collects on the wake hands a busy encoder
/// no repeat and sends as many fresh frames as today's.
#[test]
fn a_late_frame_behind_a_busy_encoder_gets_no_repeat() {
    let ms = |x: f64| Duration::from_secs_f64(x / 1e3);
    let phases = [ms(0.8), ms(0.8), ms(0.8), ms(7.8)];
    let run_for = Duration::from_millis(2000);
    let today = run_fed(120, ms(12.0), true, false, run_for, Some(&phases));
    let woken = run_fed(120, ms(12.0), true, true, run_for, Some(&phases));
    for (name, r) in [("today", &today), ("au wake", &woken)] {
        println!(
            "buffer-return source, {name}: ticks {} fresh {} repeats {} ({} on a busy encoder) \
             AU wakes {} AU age p50 {:.2} ms",
            r.ticks,
            r.fresh,
            r.repeats,
            r.busy_repeats,
            r.encoded,
            pct(&r.ages, 0.5)
        );
    }
    assert!(woken.encoded > 0, "the wake ended waits");
    assert_eq!(
        woken.busy_repeats, 0,
        "repeats handed to a busy encoder with the wake ({} repeats, {} today)",
        woken.repeats, today.repeats
    );
    assert_eq!(woken.moved_gates, 0, "a resumed wait moved");
    assert!(
        woken.fresh * 100 >= today.fresh * 97,
        "fresh frames {} with the wake, {} today",
        woken.fresh,
        today.fresh
    );
    let age = pct(&woken.ages, 0.5);
    assert!(age < 1.5, "AU age p50 {age:.2} ms with the wake");
}

/// Sync retrieve (not escalated): the encoder rings nothing, so the loop waits once a tick,
/// takes every AU in its own tick, and ticks at the rate it did.
#[test]
fn sync_retrieve_waits_once_a_tick() {
    let hz = 120;
    let r = run(
        hz,
        Duration::from_millis(3),
        false,
        true,
        Duration::from_millis(500),
    );
    println!(
        "{hz} Hz sync, encode 3 ms: AU age p50 {:.2} max {:.2} ms; ticks {} fresh {} AU wakes {}",
        pct(&r.ages, 0.5),
        pct(&r.ages, 1.0),
        r.ticks,
        r.fresh,
        r.encoded
    );
    assert_eq!(r.encoded, 0);
    assert_eq!(r.submits, r.ticks);
    assert!(
        pct(&r.ages, 0.5) < 1.5,
        "a sync AU goes out in its own tick"
    );
}
