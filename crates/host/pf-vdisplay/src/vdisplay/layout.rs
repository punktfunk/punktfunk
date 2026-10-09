//! Pure display-arrangement: members in acquire order plus a [`Layout`] policy
//! become each member's top-left origin. No I/O, no OS types. The registry
//! readout and the per-backend position apply both consume this, so the math
//! lives in one place. Design: `design/display-management.md`.
//!
//! Auto-row is left-to-right, top-aligned: member *i* at `x = Σ widths[0..i]`,
//! `y = 0`. Manual sits a member on its identity-slot pin, or packs it
//! left-to-right past the rightmost pinned edge. Overlapping pins are copied
//! verbatim.
//!
//! Members have no height, so clearance is x-only and every pin counts
//! regardless of `y`. A vertical stack therefore packs further right than it
//! needs to: a gap wastes coordinate space; an overlap maps two desktops onto
//! the same pixels.
//!
//! Group membership lives in [`super::registry`].

use super::policy::{Layout, LayoutMode};

/// One group member, in acquire order.
#[derive(Clone, Copy, Debug)]
pub struct Member {
    /// Manual-layout key. `None` cannot carry a pin, so it always auto-rows.
    pub identity_slot: Option<u32>,
    /// Width in the same coordinate space as the resulting [`Placement`].
    /// Clamped at 0 so a negative never shifts a sibling left.
    ///
    /// Fill sites pass mode pixels. That matches Windows CCD; KWin
    /// `config.position()` is logical, so a scaled output is too wide unless
    /// the fill site divides by applied scale. This type stays unit-agnostic.
    pub width: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub x: i32,
    pub y: i32,
}

/// Prefix-sum of prior widths, top-aligned. Saturating: a client-supplied
/// absurd width must not panic the state readout.
fn auto_row_x(members: &[Member], i: usize) -> i32 {
    members[..i]
        .iter()
        .fold(0i32, |x, m| x.saturating_add(m.width.max(0)))
}

/// Pin for `m` if its slot is in [`Layout::positions`]. Lookup is the canonical
/// decimal slot string — `DisplayPolicy::sanitized` re-keys `"01"` on write.
fn pin_of(m: &Member, layout: &Layout) -> Option<Placement> {
    m.identity_slot
        .and_then(|slot| layout.positions.get(&slot.to_string()))
        .map(|p| Placement { x: p.x, y: p.y })
}

/// One [`Placement`] per member, same order. Shared by the state readout and
/// (KWin) the per-backend position apply.
pub fn arrange(members: &[Member], layout: &Layout) -> Vec<Placement> {
    match layout.mode {
        LayoutMode::AutoRow => (0..members.len())
            .map(|i| Placement {
                x: auto_row_x(members, i),
                y: 0,
            })
            .collect(),
        LayoutMode::Manual => arrange_manual(members, layout),
    }
}

/// Pins verbatim; unpinned members row out past the rightmost pinned edge.
///
/// The cursor is seeded from every pin, so an already-placed unpinned `x`
/// shifts when a pinned sibling arrives later in acquire order. Nothing here
/// re-applies: `registry::position_for_new` takes only `.last()` and moves the
/// new display. The registry must re-apply the whole group on a Manual
/// membership change (`windows/manager.rs` `arrange_slots` already does).
/// Seeding from preceding pins only restores incremental stability by
/// bringing the overlap back — the wrong trade.
fn arrange_manual(members: &[Member], layout: &Layout) -> Vec<Placement> {
    let pins: Vec<Option<Placement>> = members.iter().map(|m| pin_of(m, layout)).collect();
    // `max(0)` so a negative width cannot pull the cursor back over a pin.
    let mut cursor = pins
        .iter()
        .zip(members)
        .filter_map(|(pin, m)| pin.map(|p| p.x.saturating_add(m.width.max(0))))
        .fold(0i32, i32::max);
    pins.iter()
        .zip(members)
        .map(|(pin, m)| match pin {
            Some(p) => *p,
            None => {
                let at = Placement { x: cursor, y: 0 };
                cursor = cursor.saturating_add(m.width.max(0));
                at
            }
        })
        .collect()
}

/// One lit output in compositor-logical space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Do two rects share a pixel? Edge-to-edge is not overlap.
fn overlaps(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

/// Origin for the streamed output, given every output that stays lit beside it
/// (`lit`) and its own current rect. `None` keeps the compositor's arrangement.
///
/// Alone it takes the desktop origin: a compositor appends a new output to the
/// right of the row and need not re-normalize when the row goes dark, and a lone
/// screen whose origin is not zero makes KDE place popups off it. Beside a lit
/// screen the arrangement stands — unless a stored origin sits us on top of one.
pub fn origin_for(lit: &[Rect], ours: Option<Rect>) -> Option<Placement> {
    if lit.is_empty() {
        return Some(Placement { x: 0, y: 0 });
    }
    let ours = ours?;
    lit.iter().any(|s| overlaps(ours, *s)).then(|| Placement {
        x: lit
            .iter()
            .map(|r| r.x.saturating_add(r.w.max(0)))
            .max()
            .unwrap_or(0),
        y: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Position;
    use std::collections::BTreeMap;

    fn r(x: i32, w: i32) -> Rect {
        Rect {
            x,
            y: 0,
            w,
            h: 1080,
        }
    }

    #[test]
    fn a_lone_streamed_output_takes_the_desktop_origin() {
        // Every physical is dark: KWin left ours parked right of the row it appended to.
        assert_eq!(
            origin_for(&[], Some(r(3840, 1920))),
            Some(Placement { x: 0, y: 0 })
        );
    }

    #[test]
    fn a_lit_screen_keeps_its_arrangement() {
        // Ours sits to the right of the physical — nothing to repair.
        assert_eq!(origin_for(&[r(0, 3840)], Some(r(3840, 1920))), None);
        // Unknown geometry is not a reason to move a screen that is not alone.
        assert_eq!(origin_for(&[r(0, 3840)], None), None);
    }

    #[test]
    fn a_stored_origin_never_stacks_us_on_a_lit_screen() {
        // An earlier exclusive session stored (0, 0) for our name; the physical is back.
        assert_eq!(
            origin_for(&[r(0, 3840)], Some(r(0, 1920))),
            Some(Placement { x: 3840, y: 0 })
        );
        // Past the rightmost edge of every lit screen, not just the one we hit.
        assert_eq!(
            origin_for(&[r(0, 2560), r(2560, 1920)], Some(r(0, 1920))),
            Some(Placement { x: 4480, y: 0 })
        );
    }

    #[test]
    fn touching_edges_do_not_count_as_overlap() {
        assert!(!overlaps(r(0, 1920), r(1920, 1280)));
        assert!(overlaps(r(0, 1920), r(1919, 1280)));
    }

    fn m(slot: Option<u32>, width: i32) -> Member {
        Member {
            identity_slot: slot,
            width,
        }
    }

    fn manual(pairs: &[(&str, i32, i32)]) -> Layout {
        let mut positions = BTreeMap::new();
        for (k, x, y) in pairs {
            positions.insert(k.to_string(), Position { x: *x, y: *y });
        }
        Layout {
            mode: LayoutMode::Manual,
            positions,
        }
    }

    #[test]
    fn auto_row_accumulates_widths_top_aligned() {
        let members = [m(Some(1), 2560), m(Some(2), 1920), m(None, 1280)];
        let out = arrange(&members, &Layout::default());
        assert_eq!(
            out,
            vec![
                Placement { x: 0, y: 0 },
                Placement { x: 2560, y: 0 },
                Placement { x: 4480, y: 0 },
            ]
        );
    }

    #[test]
    fn manual_honors_pins_by_identity_slot() {
        let members = [m(Some(1), 2560), m(Some(7), 1920)];
        // Crossing order: 7 is left of 1, the reverse of auto-row.
        let layout = manual(&[("1", 1920, 0), ("7", 0, 0)]);
        let out = arrange(&members, &layout);
        assert_eq!(out[0], Placement { x: 1920, y: 0 });
        assert_eq!(out[1], Placement { x: 0, y: 0 });
    }

    #[test]
    fn manual_unpinned_and_slotless_pack_clear_of_the_pins() {
        let members = [m(Some(1), 2560), m(Some(9), 1920), m(None, 1280)];
        let layout = manual(&[("1", 100, 50)]);
        let out = arrange(&members, &layout);
        assert_eq!(out[0], Placement { x: 100, y: 50 }, "pinned");
        // Pin occupies [100, 2660). Pin-blind prefix-sum would put slot 9 at
        // 2560, inside the pin.
        assert_eq!(
            out[1],
            Placement { x: 2660, y: 0 },
            "unpinned → past the pin"
        );
        assert_eq!(out[2], Placement { x: 4580, y: 0 }, "slotless → past both");
    }

    #[test]
    fn manual_with_no_pins_at_all_is_plain_auto_row() {
        let members = [m(Some(1), 2560), m(Some(2), 1920), m(None, 1280)];
        let out = arrange(&members, &manual(&[]));
        assert_eq!(out, arrange(&members, &Layout::default()));
    }

    #[test]
    fn a_manual_pin_that_would_collide_with_an_auto_row_sibling_is_packed_clear() {
        // Pin sits where pin-blind auto-row would place the sibling.
        let members = [m(Some(1), 2560), m(Some(9), 1920)];
        let layout = manual(&[("1", 2560, 0)]);
        let out = arrange(&members, &layout);
        assert_eq!(out[0], Placement { x: 2560, y: 0 }, "pin honored verbatim");
        assert_ne!(
            out[1], out[0],
            "the unpinned sibling must not land on the pin"
        );
        assert_eq!(
            out[1],
            Placement { x: 5120, y: 0 },
            "past the pin's right edge"
        );
    }

    #[test]
    fn a_pin_left_of_the_origin_still_leaves_the_unpinned_row_at_zero() {
        // KWin global space extends left of 0. Right edge -3000+2560 = -440 is
        // still left of origin, so the unpinned row stays at 0.
        let members = [m(Some(1), 2560), m(Some(9), 1920)];
        let out = arrange(&members, &manual(&[("1", -3000, 0)]));
        assert_eq!(out[0], Placement { x: -3000, y: 0 });
        assert_eq!(out[1], Placement { x: 0, y: 0 });
    }

    #[test]
    fn absurd_widths_saturate_instead_of_panicking() {
        let members = [m(Some(1), i32::MAX), m(Some(2), i32::MAX), m(None, 4096)];
        let out = arrange(&members, &Layout::default());
        assert_eq!(out[2], Placement { x: i32::MAX, y: 0 });
        let out = arrange(&members, &manual(&[("1", i32::MAX, 0)]));
        assert_eq!(out[2], Placement { x: i32::MAX, y: 0 });
    }

    /// No unpinned member may share an x-interval with any sibling. Overlap
    /// between two pins is excluded — that is the operator's arrangement.
    /// No height, so "share space" is x-only.
    #[test]
    fn no_unpinned_member_overlaps_a_sibling_under_any_layout() {
        // Numerical Recipes LCG — reproducible, no crate.
        let mut rng: u64 = 0x0bad_f00d_dead_beef;
        let mut next = || {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (rng >> 33) as u32
        };

        for _ in 0..20_000 {
            let count = (next() % 6) as usize;
            let members: Vec<Member> = (0..count)
                .map(|_| {
                    // Small slot pool so pin-table collisions are frequent.
                    let slot = match next() % 4 {
                        0 => None,
                        _ => Some(next() % 6 + 1),
                    };
                    let width = match next() % 8 {
                        0 => 0,
                        1 => -((next() % 4000) as i32),
                        _ => (next() % 4000) as i32,
                    };
                    m(slot, width)
                })
                .collect();
            let mut pairs: Vec<(String, i32, i32)> = Vec::new();
            for slot in 1..=6u32 {
                if next() % 2 == 0 {
                    let x = (next() % 8000) as i32 - 2000;
                    let y = ((next() % 3) * 1440) as i32;
                    pairs.push((slot.to_string(), x, y));
                }
            }
            let borrowed: Vec<(&str, i32, i32)> =
                pairs.iter().map(|(k, x, y)| (k.as_str(), *x, *y)).collect();

            for layout in [Layout::default(), manual(&borrowed)] {
                let out = arrange(&members, &layout);
                assert_eq!(out.len(), members.len());
                let pinned: Vec<bool> = members
                    .iter()
                    .map(|mem| pin_of(mem, &layout).is_some())
                    .collect();
                for i in 0..out.len() {
                    for j in (i + 1)..out.len() {
                        if pinned[i] && pinned[j] {
                            continue; // the operator's own arrangement
                        }
                        let span = |k: usize| {
                            let x = out[k].x as i64;
                            (x, x + members[k].width.max(0) as i64)
                        };
                        let (ai, bi) = span(i);
                        let (aj, bj) = span(j);
                        // Zero/negative width is an empty span; it cannot overlap.
                        if ai >= bi || aj >= bj {
                            continue;
                        }
                        assert!(
                            bi <= aj || bj <= ai,
                            "members {i} {:?} and {j} {:?} overlap under {layout:?} \
                             (widths {} / {})",
                            out[i],
                            out[j],
                            members[i].width,
                            members[j].width
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn empty_group_is_empty() {
        assert!(arrange(&[], &Layout::default()).is_empty());
        assert!(arrange(&[], &manual(&[("1", 0, 0)])).is_empty());
    }

    #[test]
    fn negative_width_never_shifts_siblings_left() {
        let members = [m(Some(1), -100), m(Some(2), 1920)];
        let out = arrange(&members, &Layout::default());
        let origin = Placement { x: 0, y: 0 };
        assert_eq!(out[0], origin);
        assert_eq!(out[1], origin, "clamped width contributes 0");
    }
}
