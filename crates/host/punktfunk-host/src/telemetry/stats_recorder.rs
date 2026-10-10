//! Shared streaming-stats recorder (`design/stats-capture-plan.md`). One
//! [`StatsRecorder`] is created in `host::serve` and shared with
//! [`crate::mgmt`] and the native / GameStream encode loops.
//!
//! Captures persist as JSON under the captures dir and survive a host restart.
//! [`StatsRecorder::is_armed`] is a `Relaxed` load per frame; samples are built
//! only at the loops' ~2 s / ~1 s aggregation boundary, never per frame.
//! Memory is bounded ([`MAX_SAMPLES`]). The on-disk write is temp + rename.
//! Capture ids are charset-gated so `dir.join` cannot leave the captures dir.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use utoipa::ToSchema;

/// ≈ 3 h at one sample / 2 s. Overflow stops appending (oldest kept) so a
/// recording keeps its start and never grows unbounded.
const MAX_SAMPLES: usize = 5400;

/// One stage's p50/p99 in an aggregation window (microseconds).
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct StageTiming {
    /// Pipeline order, named per path. Linux native: `queue capture submit encode send`.
    /// Windows driver: `pool encode ipc copy send`, or `driver copy send` from a driver that
    /// does not stamp its slots. GameStream: `capture encode packetize send send_spread`, with
    /// the same two driver sets on Windows.
    pub name: String,
    pub p50_us: f32,
    pub p99_us: f32,
}

/// One stage's p50/p99 over a window's samples.
pub(crate) fn stage(name: &str, v: &mut [u32]) -> StageTiming {
    StageTiming {
        name: name.into(),
        p50_us: crate::send_pacing::percentile(v, 0.50) as f32,
        p99_us: crate::send_pacing::percentile(v, 0.99) as f32,
    }
}

/// One Windows-driver AU's stages, from the driver encoder's telemetry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DriverSample {
    /// The driver stamped its slot: pool wait (`None` = unmeasured), its encode, the hand-off.
    Split {
        pool: Option<u32>,
        encode: u32,
        ipc: u32,
    },
    /// Present → arrival in one lump, from a driver that does not stamp. `None` = unmeasured.
    Lump(Option<u32>),
}

impl DriverSample {
    pub(crate) fn from_telemetry(t: Option<&pf_frame::health::EncoderTelemetry>) -> DriverSample {
        let us = |d: std::time::Duration| d.as_micros().min(u128::from(u32::MAX)) as u32;
        match t.and_then(|t| t.driver_split) {
            Some(s) => DriverSample::Split {
                pool: s.pool.map(us),
                encode: us(s.encode),
                ipc: us(s.ipc),
            },
            None => DriverSample::Lump(t.and_then(|t| t.present_to_arrival).map(us)),
        }
    }

    /// `(queue, encode)` µs for the per-AU host-timing stages; unmeasured reads 0.
    pub(crate) fn queue_encode_us(self) -> (u32, u32) {
        match self {
            DriverSample::Split { pool, encode, .. } => (pool.unwrap_or(0), encode),
            DriverSample::Lump(lump) => (0, lump.unwrap_or(0)),
        }
    }
}

/// A stats window's driver stages. An unmeasured value adds nothing, never a zero.
#[derive(Default)]
pub(crate) struct DriverStages {
    /// Some AU this window came from the driver.
    path: bool,
    /// The driver stamped its split on some AU this window.
    split: bool,
    pool: Vec<u32>,
    encode: Vec<u32>,
    ipc: Vec<u32>,
    lump: Vec<u32>,
}

impl DriverStages {
    pub(crate) fn note(&mut self, sample: DriverSample) {
        self.path = true;
        match sample {
            DriverSample::Split { pool, encode, ipc } => {
                self.split = true;
                self.pool.extend(pool);
                self.encode.push(encode);
                self.ipc.push(ipc);
            }
            DriverSample::Lump(lump) => self.lump.extend(lump),
        }
    }

    /// Some AU this window came from the driver.
    pub(crate) fn active(&self) -> bool {
        self.path
    }

    /// The window's leading stages: `pool encode ipc`, or `driver` from a driver that does
    /// not stamp. `None` off the driver path, where the plane's host stages apply.
    pub(crate) fn stages(&mut self) -> Option<Vec<StageTiming>> {
        if self.split {
            Some(vec![
                stage("pool", &mut self.pool),
                stage("encode", &mut self.encode),
                stage("ipc", &mut self.ipc),
            ])
        } else if self.path {
            Some(vec![stage("driver", &mut self.lump)])
        } else {
            None
        }
    }

    /// Start the next window.
    #[cfg_attr(not(feature = "gamestream"), allow(dead_code, reason = "compat plane"))]
    pub(crate) fn reset(&mut self) {
        *self = DriverStages::default();
    }
}

/// One aggregated sample (~2 s native, ~1 s GameStream).
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct StatsSample {
    /// Milliseconds since capture start (monotonic; stamped by [`StatsRecorder::push_sample`]).
    pub t_ms: u64,
    /// Distinguishes concurrent sessions (usually constant for one loop).
    pub session_id: u32,
    pub stages: Vec<StageTiming>,
    /// Genuine new frames/s from the source (not including repeats).
    pub fps: f32,
    /// Re-encoded holds/s — source starvation, not new frames.
    pub repeat_fps: f32,
    /// Attempted sealed wire Mb/s at seal time (AU + shard framing + FEC, including
    /// datagram-aligned zero-pad). Not goodput; socket send drops do not reduce it.
    pub mbps: f32,
    pub bitrate_kbps: u32,
    /// Counters are deltas for this window. `None` = this path cannot see it, never a zero it
    /// did not measure. Frames the host dropped: the driver's pool, or GameStream's queue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = u32, required = false)]
    pub frames_dropped: Option<u32>,
    /// Receiver-side loss. Only a client measures it; recordings older than this field hold 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = u32, required = false)]
    pub packets_dropped: Option<u32>,
    /// Host send-buffer overflow / EAGAIN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = u32, required = false)]
    pub send_dropped: Option<u32>,
    /// FEC shards the receiver recovered. Only a client measures it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = u32, required = false)]
    pub fec_recovered: Option<u32>,
    /// Capture → fully sent, p50/p99 µs: the span a client's `host` term reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = f32, required = false)]
    pub host_p50_us: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = f32, required = false)]
    pub host_p99_us: Option<f32>,
    /// Smoothed QUIC round trip to the client, µs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = u32, required = false)]
    pub rtt_us: Option<u32>,
    /// Sealing one frame, mean µs over the window: FEC parity, AES-GCM, the socket sends.
    /// Native only; timed while a capture runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = f32, required = false)]
    pub fec_us: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = f32, required = false)]
    pub seal_us: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = f32, required = false)]
    pub sock_us: Option<f32>,
}

/// Filename stem plus negotiated mode/codec/client. On-disk head;
/// [`StatsRecorder::list`] returns this without the sample body.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct CaptureMeta {
    /// Filename stem, e.g. `2026-06-26T20-14-03Z_5120x1440`.
    pub id: String,
    pub started_unix_ms: u64,
    pub duration_ms: u64,
    /// `"native" | "gamestream" | "web"`.
    pub kind: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// `"h264" | "hevc" | "av1"`.
    pub codec: String,
    /// Fingerprint prefix, or `""` if unknown.
    pub client: String,
    pub sample_count: u32,
    /// Backend that actually opened (`"nvenc"`, `"vaapi"`, `"vulkan"`, `"amf"`,
    /// `"qsv"`, `"software"`, …), from `pf_gpu::active()`. Stage timings are
    /// unreadable without it. `""` if nothing was streaming at registration.
    #[serde(default)]
    pub encoder_backend: String,
    /// GPU name from `pf_gpu::active()`, or `""`.
    #[serde(default)]
    pub gpu: String,
    /// The sample cap was hit: samples past it were dropped and the start kept.
    #[serde(default)]
    pub truncated: bool,
}

/// Wire and on-disk shape: summary plus sample time-series.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct Capture {
    pub meta: CaptureMeta,
    pub samples: Vec<StatsSample>,
    /// One row per minute of link health per native session, so a freeze report carries the
    /// loss, recovery and ABR history instead of a log ring that holds 45 minutes. Empty on a
    /// GameStream capture and on recordings older than this field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub link: Vec<crate::link_health::LinkMinute>,
}

/// In-progress capture, as the management API reports it.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct StatsStatus {
    pub armed: bool,
    pub sample_count: u32,
    /// Unix start of the in-progress capture (`0` if idle).
    pub started_unix_ms: u64,
    /// Host monotonic elapsed ms (`0` if idle). Do not subtract `started_unix_ms`
    /// from the console's wall clock — that clock may be skewed.
    pub elapsed_ms: u64,
    /// `"native" | "gamestream" | "web"`, or `""` if idle.
    pub kind: String,
}

/// Mode/codec/client from the first [`StatsRecorder::register_session`] of a capture.
#[derive(Clone)]
struct MetaSeed {
    kind: String,
    width: u32,
    height: u32,
    fps: u32,
    codec: String,
    client: String,
    encoder_backend: String,
    gpu: String,
}

/// In-progress capture (present iff armed).
struct Live {
    /// Monotonic origin for sample `t_ms`.
    started: Instant,
    started_unix_ms: u64,
    /// Seeded once, on the first session registration.
    meta: Option<MetaSeed>,
    samples: Vec<StatsSample>,
    /// One row a minute per session; the same cap bounds it.
    link: Vec<crate::link_health::LinkMinute>,
    /// Sample cap was hit; further samples are dropped.
    truncated: bool,
}

pub struct StatsRecorder {
    dir: PathBuf,
    /// Hot-path gate: `Relaxed` load per frame; never blocks the frame thread.
    armed: AtomicBool,
    /// In-progress capture. Poison recovers (`into_inner`) so a stats panic
    /// cannot kill a healthy stream.
    live: Mutex<Option<Live>>,
    next_sid: AtomicU32,
    /// Bumped per fresh capture, so a loop that outlives a capture registers again.
    generation: AtomicU64,
}

/// `~/.config/punktfunk/captures/`, via the same config-dir helper as `cert.pem`.
pub fn default_dir() -> PathBuf {
    pf_paths::config_dir().join("captures")
}

/// Charset `^[A-Za-z0-9._-]+$` (what [`capture_id`] emits; dashes not colons
/// so the stem is a Windows filename). Also reject `.` / `..` — the charset
/// allows bare dots. `/` and `\` are already excluded, so `dir.join` is one child.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Filesystem-safe id from start time + resolution, e.g.
/// `2026-06-26T20-14-03Z_5120x1440`. Dashes, not colons, so Windows accepts it.
fn capture_id(unix_ms: u64, width: u32, height: u32) -> String {
    let stamp = punktfunk_core::time::utc_rfc3339(unix_ms, false).replace(':', "-");
    format!("{stamp}_{width}x{height}")
}

impl StatsRecorder {
    /// Create `dir` owner-private (best-effort) if missing.
    pub fn new(dir: PathBuf) -> Arc<Self> {
        if let Err(e) = pf_paths::create_private_dir(&dir) {
            tracing::warn!(dir = %dir.display(), error = %e, "stats captures dir not created");
        }
        Arc::new(StatsRecorder {
            dir,
            armed: AtomicBool::new(false),
            live: Mutex::new(None),
            next_sid: AtomicU32::new(0),
            generation: AtomicU64::new(0),
        })
    }

    /// Which capture is live. A loop caches `(generation, sid)` and calls
    /// [`Self::register_session`] again when this moves: the header is per capture.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// This loop's session id in the live capture. `cached` holds `(generation, id)`;
    /// `register` runs again whenever a new capture has started, since the header is per capture.
    pub fn session_id(
        &self,
        cached: &mut Option<(u64, u32)>,
        register: impl FnOnce() -> u32,
    ) -> u32 {
        let generation = self.generation();
        match *cached {
            Some((g, id)) if g == generation => id,
            _ => {
                let id = register();
                *cached = Some((generation, id));
                id
            }
        }
    }

    /// Per-frame `Relaxed` load: whether this frame should measure.
    pub fn is_armed(&self) -> bool {
        self.armed.load(Ordering::Relaxed)
    }

    /// Arm a new capture. No-op if already armed (does not wipe; returns status).
    pub fn start(&self) -> StatsStatus {
        let mut guard = self.live.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            *guard = Some(Live {
                started: Instant::now(),
                started_unix_ms: crate::clock::unix_ms(),
                meta: None,
                samples: Vec::new(),
                link: Vec::new(),
                truncated: false,
            });
            self.generation.fetch_add(1, Ordering::Relaxed);
            // Publish after `live` exists so a frame that sees `armed` can always push.
            self.armed.store(true, Ordering::Relaxed);
        }
        status_of(guard.as_ref())
    }

    /// First registration while armed seeds `CaptureMeta`; later ones are ignored.
    /// Returns a session id to stamp on this loop's samples.
    pub fn register_session(
        &self,
        kind: &'static str,
        w: u32,
        h: u32,
        fps: u32,
        codec: &str,
        client: &str,
    ) -> u32 {
        let sid = self.next_sid.fetch_add(1, Ordering::Relaxed);
        // `pf_gpu::active()` takes its own lock — read it outside `live`, once per capture.
        let (encoder_backend, gpu) = pf_gpu::active()
            .map(|(g, _)| (g.backend.to_string(), g.name))
            .unwrap_or_default();
        let mut guard = self.live.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(live) = guard.as_mut() {
            if live.meta.is_none() {
                live.meta = Some(MetaSeed {
                    kind: kind.to_string(),
                    width: w,
                    height: h,
                    fps,
                    codec: codec.to_string(),
                    client: client.to_string(),
                    encoder_backend,
                    gpu,
                });
            }
        }
        sid
    }

    /// Append one aggregated sample. Restamps `t_ms` from the monotonic start
    /// (callers may leave it `0`). Stops appending at [`MAX_SAMPLES`] (oldest kept).
    /// No-op if unarmed (a `stop()` raced the frame boundary).
    pub fn push_sample(&self, session_id: u32, mut sample: StatsSample) {
        let mut guard = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let Some(live) = guard.as_mut() else { return };
        if live.samples.len() >= MAX_SAMPLES {
            if !live.truncated {
                live.truncated = true;
                tracing::warn!(
                    max = MAX_SAMPLES,
                    "stats capture hit the sample cap — further samples dropped (oldest kept)"
                );
            }
            return;
        }
        sample.session_id = session_id;
        sample.t_ms = live.started.elapsed().as_millis() as u64;
        live.samples.push(sample);
    }

    /// Append one closed link-health minute. Same cap and the same no-op-when-unarmed rule as
    /// [`Self::push_sample`]; the control task checks [`Self::is_armed`] before building one.
    pub fn push_link(&self, minute: crate::link_health::LinkMinute) {
        let mut guard = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let Some(live) = guard.as_mut() else { return };
        if live.link.len() < MAX_SAMPLES {
            live.link.push(minute);
        }
    }

    /// Disarm, write `<dir>/<id>.json` (temp + rename), return meta. `Ok(None)` if idle.
    pub fn stop(&self) -> std::io::Result<Option<CaptureMeta>> {
        // Clear the gate first so frame threads stop building samples immediately.
        self.armed.store(false, Ordering::Relaxed);
        let Some(live) = self.live.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return Ok(None);
        };
        let meta = meta_of(&live);
        let capture = Capture {
            meta: meta.clone(),
            samples: live.samples,
            link: live.link,
        };
        let bytes = serde_json::to_vec(&capture).map_err(std::io::Error::other)?;
        // `id` is generated (`valid_id`), so this names a child of `dir`.
        pf_paths::replace_file(&self.dir.join(format!("{}.json", meta.id)), &bytes)?;
        Ok(Some(meta))
    }

    /// In-progress status (idle = `armed: false`, zeroed fields).
    pub fn status(&self) -> StatsStatus {
        status_of(self.live.lock().unwrap_or_else(|e| e.into_inner()).as_ref())
    }

    /// Clone of the in-progress capture (`None` when idle).
    pub fn live_snapshot(&self) -> Option<Capture> {
        let guard = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let live = guard.as_ref()?;
        Some(Capture {
            meta: meta_of(live),
            samples: live.samples.clone(),
            link: live.link.clone(),
        })
    }

    /// Saved recordings, newest first. Parses each file's `meta` head only.
    pub fn list(&self) -> Vec<CaptureMeta> {
        /// `meta` only — serde skips the large `samples` array.
        #[derive(Deserialize)]
        struct MetaOnly {
            meta: CaptureMeta,
        }
        let mut out: Vec<CaptureMeta> = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(parsed) = serde_json::from_slice::<MetaOnly>(&bytes) {
                    out.push(parsed.meta);
                }
            }
        }
        out.sort_by_key(|m| std::cmp::Reverse(m.started_unix_ms));
        out
    }

    /// Load by id. Path-unsafe id and missing file are both `NotFound`.
    pub fn load(&self, id: &str) -> std::io::Result<Capture> {
        let path = self.recording_path(id)?;
        let bytes = std::fs::read(&path)?;
        serde_json::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Delete by id. Path-unsafe id and missing file are both `NotFound`.
    pub fn delete(&self, id: &str) -> std::io::Result<()> {
        let path = self.recording_path(id)?;
        std::fs::remove_file(&path)
    }

    /// `dir/<id>.json` after [`valid_id`]. Rejected id is `NotFound` so `join` never leaves `dir`.
    fn recording_path(&self, id: &str) -> std::io::Result<PathBuf> {
        if !valid_id(id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "invalid recording id",
            ));
        }
        Ok(self.dir.join(format!("{id}.json")))
    }
}

fn status_of(live: Option<&Live>) -> StatsStatus {
    match live {
        Some(l) => StatsStatus {
            armed: true,
            sample_count: l.samples.len() as u32,
            started_unix_ms: l.started_unix_ms,
            elapsed_ms: l.started.elapsed().as_millis() as u64,
            kind: l.meta.as_ref().map(|m| m.kind.clone()).unwrap_or_default(),
        },
        None => StatsStatus {
            armed: false,
            sample_count: 0,
            started_unix_ms: 0,
            elapsed_ms: 0,
            kind: String::new(),
        },
    }
}

/// `CaptureMeta` for a live or finalizing capture. Id from start + mode;
/// duration from the monotonic clock.
fn meta_of(live: &Live) -> CaptureMeta {
    let (kind, width, height, fps, codec, client, encoder_backend, gpu) = match &live.meta {
        Some(m) => (
            m.kind.clone(),
            m.width,
            m.height,
            m.fps,
            m.codec.clone(),
            m.client.clone(),
            m.encoder_backend.clone(),
            m.gpu.clone(),
        ),
        None => Default::default(),
    };
    CaptureMeta {
        id: capture_id(live.started_unix_ms, width, height),
        started_unix_ms: live.started_unix_ms,
        duration_ms: live.started.elapsed().as_millis() as u64,
        kind,
        width,
        height,
        fps,
        codec,
        client,
        sample_count: live.samples.len() as u32,
        encoder_backend,
        gpu,
        truncated: live.truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unmeasured lump or pool adds no sample; a measured 0 µs pool counts. Both planes
    /// feed these, so a missing present stamp cannot drag the `driver` p50 to zero.
    #[test]
    fn driver_stages_count_what_was_measured_and_nothing_else() {
        assert_eq!(DriverSample::from_telemetry(None), DriverSample::Lump(None));
        let mut d = DriverStages::default();
        assert!(d.stages().is_none());
        for lump in [None, None, Some(500)] {
            d.note(DriverSample::Lump(lump));
        }
        let lump = d.stages().expect("driver path");
        assert_eq!(lump.len(), 1);
        assert_eq!((lump[0].name.as_str(), lump[0].p50_us), ("driver", 500.0));

        d.reset();
        assert!(!d.active());
        for pool in [Some(0), Some(0), Some(900), None] {
            d.note(DriverSample::Split {
                pool,
                encode: 2_000,
                ipc: 40,
            });
        }
        let split = d.stages().expect("split path");
        let names: Vec<_> = split.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["pool", "encode", "ipc"]);
        assert_eq!(split[0].p50_us, 0.0);
        assert_eq!(
            DriverSample::Split {
                pool: None,
                encode: 2_000,
                ipc: 40
            }
            .queue_encode_us(),
            (0, 2_000)
        );
    }

    /// A loop keeps its session id for one capture and registers again for the next.
    #[test]
    fn session_id_registers_once_per_capture() {
        let dir = temp_dir();
        let rec = StatsRecorder::new(dir.clone());
        let mut sid = None;
        rec.start();
        assert_eq!(rec.session_id(&mut sid, || 7), 7);
        assert_eq!(rec.session_id(&mut sid, || unreachable!()), 7);
        let _ = rec.stop();
        rec.start();
        assert_eq!(rec.session_id(&mut sid, || 9), 9);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn temp_dir() -> PathBuf {
        // Process-wide counter, not a timestamp: parallel tests in the same
        // millisecond would share a dir and one cleanup would wipe the other.
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("pf-stats-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn sample() -> StatsSample {
        StatsSample {
            t_ms: 0,
            session_id: 0,
            stages: vec![StageTiming {
                name: "capture".into(),
                p50_us: 100.0,
                p99_us: 200.0,
            }],
            fps: 60.0,
            repeat_fps: 0.0,
            mbps: 25.0,
            bitrate_kbps: 20_000,
            frames_dropped: None,
            packets_dropped: None,
            send_dropped: Some(0),
            fec_recovered: None,
            host_p50_us: Some(3_000.0),
            host_p99_us: Some(5_000.0),
            rtt_us: None,
            fec_us: None,
            seal_us: None,
            sock_us: None,
        }
    }

    /// A loop outlives captures: the second capture of one stream must seed its own header,
    /// not save `0x0@0` with an empty path.
    #[test]
    fn a_second_capture_seeds_its_own_header() {
        let dir = temp_dir();
        let rec = StatsRecorder::new(dir.clone());
        let mut sid: Option<(u64, u32)> = None;
        let mut tick = |rec: &StatsRecorder| {
            let capture_gen = rec.generation();
            let id = match sid {
                Some((g, id)) if g == capture_gen => id,
                _ => {
                    let id = rec.register_session("native", 1920, 1080, 120, "hevc", "ab12");
                    sid = Some((capture_gen, id));
                    id
                }
            };
            rec.push_sample(id, sample());
        };
        rec.start();
        tick(&rec);
        let first = rec.stop().unwrap().unwrap();
        rec.start();
        tick(&rec);
        let second = rec.stop().unwrap().unwrap();
        assert_eq!((first.kind.as_str(), first.width), ("native", 1920));
        assert_eq!((second.kind.as_str(), second.width), ("native", 1920));
        assert!(!second.truncated);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Absent counters stay absent on disk; a recording written before them still loads.
    #[test]
    fn unmeasured_counters_are_absent_not_zero() {
        let json = serde_json::to_string(&sample()).unwrap();
        assert!(
            !json.contains("fec_recovered") && !json.contains("rtt_us"),
            "{json}"
        );
        assert!(json.contains("\"send_dropped\":0"));
        let old = r#"{"t_ms":0,"session_id":0,"stages":[],"fps":60,"repeat_fps":0,"mbps":1,
            "bitrate_kbps":1,"frames_dropped":0,"packets_dropped":0,"send_dropped":0,
            "fec_recovered":0}"#;
        let s: StatsSample = serde_json::from_str(old).unwrap();
        assert_eq!((s.fec_recovered, s.host_p50_us), (Some(0), None));
    }

    #[test]
    fn arm_record_save_load_delete() {
        let dir = temp_dir();
        let rec = StatsRecorder::new(dir.clone());
        assert!(!rec.is_armed());
        assert!(!rec.status().armed);
        rec.push_sample(0, sample());

        let st = rec.start();
        assert!(st.armed);
        assert!(rec.is_armed());
        let sid = rec.register_session("native", 5120, 1440, 240, "hevc", "abcd");
        rec.push_sample(sid, sample());
        rec.push_sample(sid, sample());
        assert_eq!(rec.status().sample_count, 2);
        assert_eq!(rec.status().kind, "native");
        assert!(rec.live_snapshot().is_some());

        let meta = rec.stop().unwrap().expect("a capture was recording");
        assert_eq!(meta.sample_count, 2);
        assert_eq!(meta.kind, "native");
        assert_eq!(meta.width, 5120);
        assert!(meta.id.ends_with("_5120x1440"), "id was {}", meta.id);
        assert!(!rec.is_armed());
        assert!(rec.live_snapshot().is_none());
        assert!(rec.stop().unwrap().is_none());

        let list = rec.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, meta.id);
        let loaded = rec.load(&meta.id).unwrap();
        assert_eq!(loaded.samples.len(), 2);
        assert_eq!(loaded.meta.codec, "hevc");

        rec.delete(&meta.id).unwrap();
        assert!(rec.list().is_empty());
        assert_eq!(
            rec.delete(&meta.id).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_path_traversal_ids() {
        let dir = temp_dir();
        let rec = StatsRecorder::new(dir.clone());
        for bad in [
            "../secret",
            "..",
            ".",
            "a/b",
            "a\\b",
            "",
            "/etc/passwd",
            "x/../../y",
        ] {
            assert_eq!(
                rec.load(bad).unwrap_err().kind(),
                std::io::ErrorKind::NotFound,
                "load({bad:?}) must be rejected as NotFound"
            );
            assert_eq!(
                rec.delete(bad).unwrap_err().kind(),
                std::io::ErrorKind::NotFound,
                "delete({bad:?}) must be rejected as NotFound"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn samples_are_bounded() {
        let dir = temp_dir();
        let rec = StatsRecorder::new(dir.clone());
        rec.start();
        for _ in 0..(MAX_SAMPLES + 50) {
            rec.push_sample(0, sample());
        }
        assert_eq!(rec.status().sample_count as usize, MAX_SAMPLES);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn start_is_idempotent_while_armed() {
        let dir = temp_dir();
        let rec = StatsRecorder::new(dir.clone());
        rec.start();
        rec.register_session("native", 1920, 1080, 60, "hevc", "");
        rec.push_sample(0, sample());
        let st = rec.start();
        assert!(st.armed);
        assert_eq!(st.sample_count, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
