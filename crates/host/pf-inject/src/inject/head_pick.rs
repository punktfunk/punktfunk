//! Which compositor head absolute pointer, touch and pen input lands on. One rule for every
//! Linux backend; each maps what its protocol reports into [`HeadFacts`] and decides for
//! itself what a miss means (libei must name a region; KWin and wlroots map the whole layout).

use crate::AbsoluteAnchor;

/// What a backend knows about one head. Absent facts stay `None`/zero and never match.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct HeadFacts<'a> {
    /// `wl_output.name`; EI regions carry none.
    pub name: Option<&'a str>,
    /// EI region `mapping_id`; Wayland outputs carry none.
    pub mapping_id: Option<&'a str>,
    /// Top-left in compositor logical space.
    pub x: i32,
    pub y: i32,
    /// Logical size; zero when the protocol reports no geometry (wlroots).
    pub logical_w: u32,
    pub logical_h: u32,
    /// Physical mode, when the protocol reports one (KWin). Without it the logical size
    /// stands in, scaled match included.
    pub mode: Option<(u32, u32)>,
}

impl HeadFacts<'_> {
    fn placed(&self) -> bool {
        self.logical_w > 0 && self.logical_h > 0
    }
}

/// The streamed head as the host published it, read in one go ([`crate::stream_target`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct StreamTarget {
    /// The session's output name ([`crate::set_stream_output`]).
    pub name: Option<String>,
    /// The streamed head's mode ([`crate::set_stream_extent`]).
    pub extent: Option<(u16, u16)>,
    /// The host-wide capture pin ([`crate::set_absolute_anchor`]).
    pub anchor: Option<AbsoluteAnchor>,
}

/// Index of the head `target` names, most identifying fact first:
/// 1. the anchor's `mapping_id`, the protocol's stream↔region key;
/// 2. the output name, the newest head when a supersede briefly leaves two;
/// 3. the anchor's origin: two heads can share a size, never a top-left;
/// 4. the streamed mode, then the event's `w×h` (the client's rect, which can be the
///    operator's monitor size), each by mode or by logical size with the scaled match;
/// 5. the sole head.
///
/// `None` otherwise: guessing the first head sends input to the operator's display.
pub(crate) fn pick(
    heads: &[HeadFacts],
    target: &StreamTarget,
    event_wh: Option<(u32, u32)>,
) -> Option<usize> {
    let anchor = target.anchor.as_ref();
    by_mapping_id(heads, anchor)
        .or_else(|| {
            let want = target.name.as_deref()?;
            heads.iter().rposition(|h| h.name == Some(want))
        })
        .or_else(|| by_origin(heads, anchor))
        .or_else(|| by_size(heads, target.extent.map(|(w, h)| (w.into(), h.into()))?))
        .or_else(|| by_size(heads, event_wh?))
        .or_else(|| (heads.len() == 1).then_some(0))
}

/// True when the anchor names one of `heads`. A miss is worth a log line: the pointer then
/// falls through to size matching.
pub(crate) fn anchor_matches(heads: &[HeadFacts], anchor: &AbsoluteAnchor) -> bool {
    by_mapping_id(heads, Some(anchor))
        .or_else(|| by_origin(heads, Some(anchor)))
        .is_some()
}

fn by_mapping_id(heads: &[HeadFacts], anchor: Option<&AbsoluteAnchor>) -> Option<usize> {
    let id = anchor?.mapping_id.as_deref()?;
    heads.iter().position(|h| h.mapping_id == Some(id))
}

fn by_origin(heads: &[HeadFacts], anchor: Option<&AbsoluteAnchor>) -> Option<usize> {
    let origin = anchor?.origin?;
    heads
        .iter()
        .position(|h| h.placed() && (h.x, h.y) == origin)
}

/// Exact first (mode, else logical size), then the scaled match on heads without a mode.
fn by_size(heads: &[HeadFacts], wh: (u32, u32)) -> Option<usize> {
    heads
        .iter()
        .position(|h| h.placed() && h.mode.unwrap_or((h.logical_w, h.logical_h)) == wh)
        .or_else(|| {
            heads
                .iter()
                .position(|h| h.mode.is_none() && scaled_match(h, wh))
        })
}

/// True when `h` is the streamed `w×h` surface at display scale > 1, which shrinks the
/// logical size (Mutter: 1280×800 at 1.5 → 853×533). ±2 logical px covers per-axis floor.
/// Scales 1..=4 only: past 4 is no real display scale and would pick the wrong monitor.
fn scaled_match(h: &HeadFacts, (w, ht): (u32, u32)) -> bool {
    if !h.placed() {
        return false;
    }
    let (rw, rh) = (h.logical_w as f32, h.logical_h as f32);
    let s = w as f32 / rw;
    (1.0..=4.0).contains(&s) && (rh * s - ht as f32).abs() <= 2.0 * s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A head with geometry and no name (an EI region).
    fn region(x: i32, y: i32, w: u32, h: u32) -> HeadFacts<'static> {
        HeadFacts {
            x,
            y,
            logical_w: w,
            logical_h: h,
            ..HeadFacts::default()
        }
    }

    /// A KWin head: name, mode and logical rect at scale 1.
    fn output(name: &'static str, x: i32, w: u32, h: u32) -> HeadFacts<'static> {
        HeadFacts {
            name: Some(name),
            mode: Some((w, h)),
            ..region(x, 0, w, h)
        }
    }

    /// A wlroots head: a name and nothing else.
    fn named(name: Option<&'static str>) -> HeadFacts<'static> {
        HeadFacts {
            name,
            ..HeadFacts::default()
        }
    }

    fn origin(x: i32, y: i32) -> StreamTarget {
        StreamTarget {
            anchor: Some(AbsoluteAnchor {
                origin: Some((x, y)),
                mapping_id: None,
            }),
            ..StreamTarget::default()
        }
    }

    fn name(n: &str) -> StreamTarget {
        StreamTarget {
            name: Some(n.into()),
            ..StreamTarget::default()
        }
    }

    fn extent(w: u16, h: u16) -> StreamTarget {
        StreamTarget {
            extent: Some((w, h)),
            ..StreamTarget::default()
        }
    }

    const NONE: StreamTarget = StreamTarget {
        name: None,
        extent: None,
        anchor: None,
    };

    /// Two heads at the same size: size matching is a coin flip. Origin picks.
    #[test]
    fn the_origin_disambiguates_two_same_size_monitors() {
        let heads = [region(0, 0, 1920, 1080), region(1920, 0, 1920, 1080)];
        assert_eq!(pick(&heads, &origin(1920, 0), Some((1920, 1080))), Some(1));
        // No anchor takes the first same-sized head — the client-sized virtual-output path.
        assert_eq!(pick(&heads, &NONE, Some((1920, 1080))), Some(0));
    }

    /// A GameStream client sends its own window rect, which can be the operator's monitor size.
    /// The streamed head's mode picks first; the event's size is the fallback.
    #[test]
    fn the_streamed_mode_outranks_the_client_rect() {
        let heads = [
            region(0, 0, 1920, 1080),    // the operator's monitor
            region(1920, 0, 3840, 2160), // the streamed head
        ];
        assert_eq!(
            pick(&heads, &extent(3840, 2160), Some((1920, 1080))),
            Some(1)
        );
        assert_eq!(pick(&heads, &NONE, Some((1920, 1080))), Some(0));
        // A published mode no head matches falls back to the event's size.
        assert_eq!(
            pick(&heads, &extent(1280, 720), Some((3840, 2160))),
            Some(1)
        );
    }

    /// `mapping_id` outranks origin: a stale or rounded origin must not override the
    /// protocol's stream↔region key.
    #[test]
    fn mapping_id_outranks_the_origin() {
        let heads = [
            HeadFacts {
                mapping_id: Some("head-a"),
                ..region(0, 0, 1920, 1080)
            },
            HeadFacts {
                mapping_id: Some("head-b"),
                ..region(1920, 0, 1920, 1080)
            },
        ];
        let target = StreamTarget {
            anchor: Some(AbsoluteAnchor {
                origin: Some((0, 0)),
                mapping_id: Some("head-b".into()),
            }),
            ..StreamTarget::default()
        };
        assert_eq!(pick(&heads, &target, Some((1920, 1080))), Some(1));
    }

    /// Display scale shrinks the logical size. 1280×800 at 1.5 is 853×533.
    #[test]
    fn a_scaled_output_matches_the_streamed_size() {
        let heads = [
            region(0, 0, 1462, 1044),
            region(1462, 0, 1920, 1080),
            region(3382, 0, 853, 533),
        ];
        assert_eq!(pick(&heads, &NONE, Some((1280, 800))), Some(2));
        let heads = [region(0, 0, 1920, 1080), region(1920, 0, 640, 400)];
        assert_eq!(pick(&heads, &NONE, Some((1280, 800))), Some(1));
        // Wrong aspect is not a consistent scale.
        let heads = [region(0, 0, 1000, 1000), region(1000, 0, 640, 200)];
        assert_eq!(pick(&heads, &NONE, Some((1280, 800))), None);
    }

    /// A mirrored monitor's region is not the streamed size; origin is what finds it.
    #[test]
    fn the_anchor_finds_a_monitor_the_streamed_size_does_not_match() {
        let heads = [region(0, 0, 1920, 1080), region(1920, 0, 3840, 2160)];
        assert_eq!(pick(&heads, &origin(1920, 0), Some((1280, 720))), Some(1));
    }

    /// An unmatched anchor falls through to size matching and is reported.
    #[test]
    fn an_unmatched_anchor_falls_back_and_is_reported() {
        let heads = [region(0, 0, 1920, 1080), region(1920, 0, 1280, 720)];
        let miss = origin(5000, 5000);
        assert!(!anchor_matches(&heads, miss.anchor.as_ref().unwrap()));
        assert_eq!(pick(&heads, &miss, Some((1920, 1080))), Some(0));
        assert!(anchor_matches(
            &heads,
            origin(0, 0).anchor.as_ref().unwrap()
        ));
    }

    /// A negative origin matches no EI region (their offsets are unsigned).
    #[test]
    fn a_negative_origin_matches_nothing() {
        let heads = [region(0, 0, 1920, 1080), region(1920, 0, 1920, 1080)];
        let miss = origin(-1920, 0);
        assert!(!anchor_matches(&heads, miss.anchor.as_ref().unwrap()));
        assert_eq!(pick(&heads, &miss, Some((1920, 1080))), Some(0));
    }

    /// A 1080p TV beside two 1080p monitors drives the streamed head, not the first
    /// monitor whose mode equals the TV panel.
    #[test]
    fn the_named_head_beats_a_size_match() {
        let heads = [
            output("DP-2", 0, 1920, 1080),
            output("DP-1", 1920, 1920, 1080),
            output("Virtual-punktfunk-1", 3840, 2560, 1440),
        ];
        let at = |t: &StreamTarget| pick(&heads, t, Some((1920, 1080)));
        assert_eq!(at(&name("Virtual-punktfunk-1")), Some(2));
        assert_eq!(at(&name("DP-1")), Some(1));
        // Unpublished or vanished: the size rungs.
        assert_eq!(at(&NONE), Some(0));
        assert_eq!(at(&name("gone")), Some(0));
    }

    /// The name outranks the origin: the session head beats the host-wide pin.
    #[test]
    fn the_name_outranks_the_origin() {
        let heads = [
            output("DP-1", 0, 1920, 1080),
            output("PF-1-1", 1920, 1920, 1080),
        ];
        let target = StreamTarget {
            name: Some("PF-1-1".into()),
            ..origin(0, 0)
        };
        assert_eq!(pick(&heads, &target, None), Some(1));
    }

    /// A supersede leaves the old head alive under the same name; the newer one is live.
    /// KWin and wlroots used to disagree here.
    #[test]
    fn a_shared_name_takes_the_newest_head() {
        let kwin = [
            output("Virtual-punktfunk-1", 0, 1920, 1080),
            output("Virtual-punktfunk-1", 1920, 3840, 2160),
        ];
        assert_eq!(
            pick(&kwin, &name("Virtual-punktfunk-1"), Some((1920, 1080))),
            Some(1)
        );
        let wlr = [
            named(Some("PF-87756-3")),
            named(Some("HDMI-A-1")),
            named(Some("PF-87756-3")),
        ];
        assert_eq!(pick(&wlr, &name("PF-87756-3"), None), Some(2));
    }

    /// Physical head first, session head later — advertisement order on a real box.
    #[test]
    fn binds_the_streamed_head_not_the_first_advertised_one() {
        let hypr = [named(Some("HDMI-A-1")), named(Some("PF-87756-3"))];
        assert_eq!(pick(&hypr, &name("PF-87756-3"), None), Some(1));
        assert_eq!(pick(&hypr, &name("HDMI-A-1"), None), Some(0));
        let sway = [
            named(Some("HEADLESS-1")),
            named(Some("DP-2")),
            named(Some("HEADLESS-2")),
        ];
        assert_eq!(pick(&sway, &name("HEADLESS-2"), None), Some(2));
        assert_eq!(pick(&sway, &name("DP-2"), None), Some(1));
    }

    /// No match among several heads is `None`, never the first-advertised head.
    #[test]
    fn an_unknown_target_picks_nothing_rather_than_the_first_head() {
        let hypr = [named(Some("HDMI-A-1")), named(Some("PF-87756-3"))];
        // The injector can open before the head exists.
        assert_eq!(pick(&hypr, &name("PF-87756-9"), None), None);
        assert_eq!(pick(&hypr, &NONE, None), None);
        // A v3 compositor: globals exist but never get a `name`.
        assert_eq!(
            pick(&[named(None), named(None)], &name("PF-87756-3"), None),
            None
        );
        assert_eq!(pick(&[], &name("PF-87756-3"), None), None);
        // Facts a backend lacks never match: wlroots heads have no geometry to anchor to.
        assert_eq!(pick(&hypr, &origin(0, 0), Some((0, 0))), None);
        // The sole head is the whole layout anyway.
        assert_eq!(pick(&hypr[..1], &name("PF-87756-9"), None), Some(0));
    }
}
