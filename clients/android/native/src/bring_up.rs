//! Where a MediaCodec decoder's bring-up starts: the ladder of present backends and low-latency
//! keys it climbs ([`bring_up_rungs`]) and its label in the rung memory ([`ladder_label`]), and
//! the decoder Kotlin's pick names ([`ranked_for`]). No codec: `bring_up` in
//! `decode/async_loop.rs` climbs the ladder, and `DecodeOptions::decoder_for` in `decode/mod.rs`
//! takes the pick.

/// One bring-up rung: `(present backend, low-latency keys)`, as [`bring_up_rungs`] lists them.
pub(crate) type Rung = (Option<bool>, Option<bool>);

/// The decoder bring-up rungs, in order, as `(present backend, low-latency keys)`.
/// The backend is `Some(overlay)` for ASC with that reader-usage profile (see `AscSink::create`'s
/// `overlay` doc), `None` for the SurfaceView presenter. The keys are `Some(aggressive)` for that
/// key profile, `None` for no low-latency key at all. See `bring_up` for why these axes, and why
/// in this order.
///
/// Consecutive duplicates are collapsed: the `present_backend` sysprop and the low-latency toggle
/// may each already have shed what a rung was going to shed, and re-running a configuration the
/// codec just refused buys nothing but another failed `start`. The first rung is always exactly
/// what the session asked for, so a device that works is never charged for this ladder.
pub(crate) fn bring_up_rungs(asc_wanted: bool, low_latency: bool) -> Vec<Rung> {
    let mut rungs = vec![
        (asc_wanted.then_some(true), Some(low_latency)),
        (asc_wanted.then_some(false), Some(low_latency)),
        (None, Some(low_latency)),
        (None, Some(false)),
        (None, None),
    ];
    rungs.dedup();
    rungs
}

/// The ladder as the rung memory keys it, so a rung is read back only on the ladder it was
/// learned on. Each rung is its backend (`o` overlay reader, `g` GPU-composited reader, `s`
/// SurfaceView) and its keys (`a` aggressive, `p` plain, `n` none), joined by `-`.
pub(crate) fn ladder_label(rungs: &[Rung]) -> String {
    let label = |&(backend, keys): &Rung| {
        let backend = match backend {
            Some(true) => 'o',
            Some(false) => 'g',
            None => 's',
        };
        let keys = match keys {
            Some(true) => 'a',
            Some(false) => 'p',
            None => 'n',
        };
        format!("{backend}{keys}")
    };
    rungs.iter().map(label).collect::<Vec<_>>().join("-")
}

/// Kotlin's pick `name`, ranked for `ranked`, when `mime` is that MIME; else the platform's
/// default, which low-latency mode off always takes.
pub(crate) fn ranked_for<'a>(name: Option<&'a str>, ranked: &str, mime: &str) -> Option<&'a str> {
    name.filter(|_| mime == ranked)
}

#[cfg(test)]
mod tests {
    use super::{bring_up_rungs, ladder_label, ranked_for};

    /// The ladder that turns a decoder which refuses to configure or start into a retry or two
    /// away from a picture. Order and de-duplication are the whole of its logic — everything else
    /// in the loop is MediaCodec I/O.
    #[test]
    fn rungs_shed_the_overlay_then_asc_then_the_keys_and_never_repeat_one() {
        // The default: shed the reader's COMPOSER_OVERLAY usage first (keeping ASC — the whole
        // point of the middle rung), then the `AImageReader` entirely, then the aggressive keys,
        // then every low-latency key.
        assert_eq!(
            bring_up_rungs(true, true),
            [
                (Some(true), Some(true)),
                (Some(false), Some(true)),
                (None, Some(true)),
                (None, Some(false)),
                (None, None)
            ]
        );
        // `present_backend=surfaceview` already shed ASC — both ASC rungs collapse away.
        assert_eq!(
            bring_up_rungs(false, true),
            [(None, Some(true)), (None, Some(false)), (None, None)]
        );
        // Low-latency mode off ⇒ the keys are already the plain set; the aggressive rung collapses.
        assert_eq!(
            bring_up_rungs(true, false),
            [
                (Some(true), Some(false)),
                (Some(false), Some(false)),
                (None, Some(false)),
                (None, None)
            ]
        );
        // Only the keys left to shed: one retry, and no second `start` of the same thing.
        assert_eq!(
            bring_up_rungs(false, false),
            [(None, Some(false)), (None, None)]
        );

        for asc in [true, false] {
            for ll in [true, false] {
                let rungs = bring_up_rungs(asc, ll);
                // A device that works must pay nothing for this ladder: rung 0 is always exactly
                // what the session asked for.
                assert_eq!(rungs[0], (asc.then_some(true), Some(ll)));
                // Every ladder ends at the most conservative configuration there is.
                assert_eq!(*rungs.last().unwrap(), (None, None));
                // Monotonic: a rung only ever sheds, never re-enables what an earlier one dropped
                // (`Option<bool>`'s Ord: `None < Some(false) < Some(true)`), so the ladder always
                // descends towards the conservative end.
                assert!(rungs
                    .windows(2)
                    .all(|w| w[1].0 <= w[0].0 && w[1].1 <= w[0].1));
            }
        }
    }

    #[test]
    fn each_ladder_has_its_own_label() {
        let labels = [(true, true), (false, true), (true, false), (false, false)]
            .map(|(asc, ll)| ladder_label(&bring_up_rungs(asc, ll)));
        assert_eq!(
            labels,
            ["oa-ga-sa-sp-sn", "sa-sp-sn", "op-gp-sp-sn", "sp-sn"]
        );
    }

    /// A codec the host moved to after the pick never gets a decoder ranked for another MIME.
    #[test]
    fn a_pick_names_only_the_mime_it_was_ranked_for() {
        let pick = Some("c2.qti.hevc.decoder.low_latency");
        assert_eq!(ranked_for(pick, "video/hevc", "video/hevc"), pick);
        assert_eq!(ranked_for(pick, "video/hevc", "video/avc"), None);
        assert_eq!(ranked_for(None, "video/hevc", "video/hevc"), None);
    }
}
