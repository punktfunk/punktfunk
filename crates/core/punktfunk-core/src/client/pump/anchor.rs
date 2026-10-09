//! The client's mode, moved by the host's `StreamConfig` at the first frame of the epoch it names.
//! A `Reconfigured` ack says a switch was accepted; the config says what each epoch delivers.

use crate::config::Mode;
use crate::quic::v2::msg::StreamConfig;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

#[derive(Debug, Default)]
pub(crate) struct ModeAnchor {
    /// The host sends a config per epoch (`FEATURE_STREAM_CONFIG` in force).
    pub(crate) on: bool,
    /// A config whose epoch has shown no frame yet.
    pending: Option<StreamConfig>,
    /// The newest epoch a frame carried.
    seen: Option<u8>,
}

impl ModeAnchor {
    /// A config arrived. `Some(mode)` when its epoch is already on screen.
    pub(crate) fn config(&mut self, cfg: StreamConfig) -> Option<Mode> {
        if self.seen == Some(cfg.epoch) {
            self.pending = None;
            return Some(cfg.mode);
        }
        self.pending = Some(cfg);
        None
    }

    /// The first frame of `epoch` arrived. `Some(mode)` when its config came first.
    pub(crate) fn frame(&mut self, epoch: u8) -> Option<Mode> {
        self.seen = Some(epoch);
        Some(self.pending.take_if(|c| c.epoch == epoch)?.mode)
    }
}

/// Point the mode slot at `mode`. A change bumps `mode_gen`, which resets mode-scoped ABR state.
pub(crate) fn apply(slot: &Mutex<Mode>, mode_gen: &AtomicU32, mode: Mode) {
    let mut m = slot.lock().unwrap();
    if *m != mode {
        *m = mode;
        mode_gen.fetch_add(1, Ordering::Relaxed);
        tracing::info!(?mode, "host delivered mode switch");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(epoch: u8, width: u32) -> StreamConfig {
        StreamConfig {
            epoch,
            mode: Mode {
                width,
                height: 720,
                refresh_hz: 60,
            },
            ..StreamConfig::default()
        }
    }

    /// The config and the epoch's first frame race; the mode moves when the second of them lands.
    #[test]
    fn the_mode_moves_when_config_and_frame_have_both_arrived() {
        let mut a = ModeAnchor::default();
        assert_eq!(a.frame(0), None);
        assert_eq!(a.config(cfg(1, 1280)), None, "no frame of epoch 1 yet");
        assert_eq!(a.frame(1).map(|m| m.width), Some(1280));
        assert_eq!(a.frame(1), None, "applied once");

        assert_eq!(a.frame(2), None, "a frame ahead of its config");
        assert_eq!(a.config(cfg(2, 1920)).map(|m| m.width), Some(1920));
    }

    /// A config for an epoch whose frames never came is replaced by the next one.
    #[test]
    fn a_skipped_epoch_never_moves_the_mode() {
        let mut a = ModeAnchor::default();
        assert_eq!(a.config(cfg(3, 800)), None);
        assert_eq!(a.config(cfg(4, 1024)), None);
        assert_eq!(a.frame(4).map(|m| m.width), Some(1024));
    }

    #[test]
    fn applying_the_same_mode_leaves_mode_gen_alone() {
        let slot = Mutex::new(cfg(0, 1280).mode);
        let mode_gen = AtomicU32::new(0);
        apply(&slot, &mode_gen, cfg(0, 1280).mode);
        assert_eq!(mode_gen.load(Ordering::Relaxed), 0);
        apply(&slot, &mode_gen, cfg(0, 1920).mode);
        assert_eq!(mode_gen.load(Ordering::Relaxed), 1);
        assert_eq!(slot.lock().unwrap().width, 1920);
    }
}
