//! Script-facing "a stream is live" marker at `$XDG_RUNTIME_DIR/punktfunk/stream`.
//!
//! The file exists iff at least one client is streaming. Scripts treat absence as
//! not-streaming and must not error. POSIX-`sh`-sourceable `KEY=value` lines:
//! integers or single-quoted strings with quotes/controls stripped. Keys are
//! `PF_STREAM_*` so sourcing cannot clobber a caller's `WIDTH`.
//!
//! Keys are only added. `PF_STREAM_SCHEMA` bumps if an existing key's meaning
//! changes. Current: `PF_STREAM=1`, `SCHEMA=1`, `WIDTH`, `HEIGHT`, `REFRESH`,
//! `HDR`, `SESSIONS`, `CLIENT` (primary = oldest live, single-quoted). Writes
//! are temp+rename; a mid-update source always sees a complete prior file.
//!
//! Both serving planes announce. One file, so `SESSIONS>1` means the mode
//! describes only the primary. Mode is the start negotiation; a mid-stream
//! resize is not republished.

#[derive(Clone, Debug)]
pub struct StreamInfo {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub hdr: bool,
    /// Sanitized before it reaches the file.
    pub client: String,
    /// Lifecycle events only — the marker's key set is stable.
    pub launch: Option<String>,
    /// Device id for the lifecycle events' hook filter; never in the marker file.
    pub fingerprint: Option<String>,
    /// On lifecycle events and hook filters; never in the marker file.
    pub plane: crate::events::Plane,
}

fn stream_ref(info: &StreamInfo) -> crate::events::StreamRef {
    crate::events::StreamRef {
        mode: crate::events::mode_str(info.width, info.height, info.refresh_hz),
        hdr: info.hdr,
        client: info.client.clone(),
        fingerprint: info.fingerprint.clone(),
        app: info.launch.clone(),
        plane: info.plane,
    }
}

/// One announced session. Drop deregisters it and rewrites or removes the marker.
#[must_use = "dropping the guard immediately retracts the stream marker"]
pub struct Guard {
    #[cfg(unix)]
    id: u64,
    /// Re-emitted as `stream.stopped` on drop.
    stream: crate::events::StreamRef,
}

/// Emits `stream.started` on every platform; the marker file is unix-only.
/// Drop emits `stream.stopped`.
pub fn announce(info: StreamInfo) -> Guard {
    crate::events::emit(crate::events::EventKind::StreamStarted {
        stream: stream_ref(&info),
    });
    #[cfg(unix)]
    {
        imp::announce(info)
    }
    #[cfg(not(unix))]
    {
        Guard {
            stream: stream_ref(&info),
        }
    }
}

#[cfg(not(unix))]
impl Drop for Guard {
    fn drop(&mut self) {
        crate::events::emit(crate::events::EventKind::StreamStopped {
            stream: self.stream.clone(),
        });
    }
}

#[cfg(unix)]
mod imp {
    use super::{Guard, StreamInfo};
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Live sessions, oldest first: `[0]` is the primary. Each id is held by its [`Guard`].
    struct Registry {
        next_id: u64,
        sessions: Vec<(u64, StreamInfo)>,
    }

    static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
        next_id: 1,
        sessions: Vec::new(),
    });

    impl Registry {
        fn insert(&mut self, info: StreamInfo) -> u64 {
            let id = self.next_id;
            self.next_id += 1;
            self.sessions.push((id, info));
            id
        }

        fn remove(&mut self, id: u64) {
            if let Some(pos) = self.sessions.iter().position(|(sid, _)| *sid == id) {
                self.sessions.remove(pos);
            }
        }
    }

    /// `$XDG_RUNTIME_DIR/punktfunk/`. Per-user tmpfs, gone on logout, so a reboot
    /// cannot leave a live marker. `/tmp` only when the runtime dir is unset.
    fn dir() -> PathBuf {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        base.join("punktfunk")
    }

    fn marker_path() -> PathBuf {
        dir().join("stream")
    }

    /// Strip quotes, controls, and spoofy bidi so the name is safe inside single quotes.
    /// Cap 96 so a hostile name cannot bloat the file.
    fn sanitize(name: &str) -> String {
        name.chars()
            .filter(|c| *c != '\'' && !c.is_control() && !crate::native_pairing::is_spoofy_char(*c))
            .take(96)
            .collect()
    }

    pub(super) fn announce(info: StreamInfo) -> Guard {
        let stream = super::stream_ref(&info);
        let mut reg = REGISTRY.lock().unwrap();
        let id = reg.insert(info);
        rewrite_to(&marker_path(), &reg);
        Guard { id, stream }
    }

    /// Caller holds the registry lock.
    fn rewrite_to(path: &std::path::Path, reg: &Registry) {
        let Some((_, primary)) = reg.sessions.first() else {
            // Missing is already the end state.
            let _ = std::fs::remove_file(path);
            return;
        };

        let body = format!(
            "# punktfunk stream marker — auto-generated. Present only while a client is streaming.\n\
             # Sourceable from a launch script: `. \"$XDG_RUNTIME_DIR/punktfunk/stream\"`.\n\
             PF_STREAM=1\n\
             PF_STREAM_SCHEMA=1\n\
             PF_STREAM_WIDTH={w}\n\
             PF_STREAM_HEIGHT={h}\n\
             PF_STREAM_REFRESH={hz}\n\
             PF_STREAM_HDR={hdr}\n\
             PF_STREAM_SESSIONS={n}\n\
             PF_STREAM_CLIENT='{client}'\n",
            w = primary.width,
            h = primary.height,
            hz = primary.refresh_hz,
            hdr = u8::from(primary.hdr),
            n = reg.sessions.len(),
            client = sanitize(&primary.client),
        );

        if let Err(e) = write_atomic(path, body.as_bytes()) {
            tracing::debug!(error = %e, path = %path.display(), "stream marker not written");
        }
    }

    /// Temp + rename. The registry lock serializes writers, so a fixed temp name is safe.
    fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            {
                let mut reg = REGISTRY.lock().unwrap();
                reg.remove(self.id);
                rewrite_to(&marker_path(), &reg);
            }
            crate::events::emit(crate::events::EventKind::StreamStopped {
                stream: self.stream.clone(),
            });
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn sanitize_strips_quotes_and_controls() {
            assert_eq!(sanitize("Living Room TV"), "Living Room TV");
            assert_eq!(sanitize("evil';rm -rf ~\n"), "evil;rm -rf ~");
            assert_eq!(sanitize(&"x".repeat(200)).len(), 96);
            assert_eq!(sanitize(""), "");
        }

        // Local registry at an explicit path. The process-global one is shared with
        // native integration tests; mutating XDG_RUNTIME_DIR would race them.
        #[test]
        fn marker_appears_while_held_and_vanishes_after() {
            let dir = std::env::temp_dir().join(format!("pf-marker-test-{}", std::process::id()));
            let path = dir.join("stream");
            let _ = std::fs::remove_file(&path);
            let mut reg = Registry {
                next_id: 1,
                sessions: Vec::new(),
            };

            let g = reg.insert(StreamInfo {
                width: 2560,
                height: 1440,
                refresh_hz: 120,
                hdr: true,
                client: "Couch'TV".to_string(),
                fingerprint: None,
                launch: None,
                plane: crate::events::Plane::Native,
            });
            rewrite_to(&path, &reg);
            let text = std::fs::read_to_string(&path).expect("marker exists while streaming");
            assert!(text.contains("PF_STREAM_WIDTH=2560"));
            assert!(text.contains("PF_STREAM_HEIGHT=1440"));
            assert!(text.contains("PF_STREAM_REFRESH=120"));
            assert!(text.contains("PF_STREAM_HDR=1"));
            assert!(text.contains("PF_STREAM_SESSIONS=1"));
            assert!(text.contains("PF_STREAM_CLIENT='CouchTV'"));

            let g2 = reg.insert(StreamInfo {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
                hdr: false,
                client: "Phone".to_string(),
                fingerprint: None,
                launch: None,
                plane: crate::events::Plane::Gamestream,
            });
            rewrite_to(&path, &reg);
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(text.contains("PF_STREAM_SESSIONS=2"));
            assert!(
                text.contains("PF_STREAM_WIDTH=2560"),
                "primary mode is retained"
            );

            reg.remove(g2);
            rewrite_to(&path, &reg);
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(text.contains("PF_STREAM_SESSIONS=1"));

            reg.remove(g);
            rewrite_to(&path, &reg);
            assert!(!path.exists(), "marker removed once the last session ends");
            let _ = std::fs::remove_dir(&dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Plane;

    #[test]
    fn stream_ref_carries_the_announcing_plane() {
        let info = StreamInfo {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            hdr: false,
            client: String::new(),
            fingerprint: None,
            launch: Some("Hades".to_string()),
            plane: Plane::Gamestream,
        };
        let r = stream_ref(&info);
        assert_eq!(r.plane, Plane::Gamestream);
        assert_eq!(r.mode, "1920x1080@60");
        assert_eq!(r.app.as_deref(), Some("Hades"));

        let native = stream_ref(&StreamInfo {
            plane: Plane::Native,
            ..info
        });
        assert_eq!(native.plane, Plane::Native);
    }
}
