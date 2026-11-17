//! What the swapchain is for, decided in one place: who owns the window, the HDR state, the
//! formats, the present pref and mode, the extent. [`step`] answers a [`Plan`];
//! `vk::Presenter` commits the state it returns, then performs the plan, and feeds back as
//! a further input what only Vulkan answers: the picked mode, the flipped format, a
//! swapchain down or standing again.
//!
//! Pure, and generic over the surface format and the present mode, so the table runs on
//! every platform. Ownership runs `Swapchain → Lane { shown: false } → Lane { shown: true }`,
//! and back once a real frame, or a redraw with an overlay the lane cannot show, needs the
//! swapchain. A lane state means the swapchain and its surface are torn down. A failed build
//! leaves no swapchain standing either; the next frame rebuilds. Minimized is a zero extent.
//!
//! Pinned by the table tests below.

/// Who presents into the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Owner {
    Swapchain,
    /// The native lane; swapchain and surface are torn down. `shown`: the last frame
    /// went through the lane, `pq` with a PQ description.
    Lane {
        shown: bool,
        pq: bool,
    },
}

impl Owner {
    /// The swapchain and its surface are torn down while the lane owns the window.
    pub(crate) fn suspended(self) -> bool {
        matches!(self, Owner::Lane { .. })
    }

    /// The last frame shown went through the native lane, not the swapchain.
    pub(crate) fn native_last(self) -> bool {
        matches!(self, Owner::Lane { shown: true, .. })
    }

    /// The lane's last frame is PQ, shown through the compositor's colour management.
    pub(crate) fn native_pq(self) -> bool {
        self == Owner::Lane {
            shown: true,
            pq: true,
        }
    }
}

/// The player's present intent: the settings a present-mode pick reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Pref {
    pub(crate) vsync: bool,
    pub(crate) allow_vrr: bool,
    pub(crate) fullscreen: bool,
}

/// The swapchain's configuration. `F` is the surface format, `M` the present mode.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Reconfig<F, M> {
    pub(crate) owner: Owner,
    pub(crate) hdr_active: bool,
    /// The swapchain's format.
    pub(crate) format: F,
    /// `None`: the surface offers no HDR10, or refused the swapchain.
    pub(crate) hdr10: Option<F>,
    pub(crate) pref: Pref,
    pub(crate) present_mode: M,
    /// The swapchain's extent; under the lane, the window's last pixel size.
    pub(crate) extent: (u32, u32),
    /// A swapchain stands. Not under the lane, nor after a failed build: the create retired
    /// the old swapchain whether or not it made a new one.
    pub(crate) standing: bool,
}

impl<F, M> Reconfig<F, M> {
    pub(crate) fn minimized(&self) -> bool {
        self.extent.0 == 0 || self.extent.1 == 0
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Input<F, M> {
    /// SDL's pixel size changed; (0, 0) is minimized.
    Resize {
        extent: (u32, u32),
    },
    SetPref(Pref),
    /// The surface answered `mode` for `pref`.
    Picked {
        pref: Pref,
        mode: M,
    },
    /// A frame's colour signalling.
    Follow {
        pq: bool,
    },
    /// The console takes the screen.
    LeaveHdr,
    /// The passes stand on `format`: the swapchain follows.
    Flipped {
        on: bool,
        format: F,
    },
    /// The driver refused the HDR10 swapchain it advertised.
    HdrRefused,
    /// A rebuild's targets stand, or not: the old ones went with the retired swapchain.
    Rebuilt {
        ok: bool,
    },
    /// A frame that needs the swapchain.
    RealFrame,
    Redraw {
        overlay_blocked: bool,
    },
    // Only the Linux lane takes the window.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    LaneTakes,
    /// The swapchain and its surface are down.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Suspended,
    /// A new surface answered `mode`.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Resumed {
        mode: M,
    },
    /// The lane showed a frame.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Shown {
        pq: bool,
    },
    OutOfDate,
    /// The present-timing result queue is full; the shell has disarmed the engine.
    QueueFull,
}

/// What the presenter performs. `Skip` performs nothing and the caller carries on (a frame
/// draws); `Drop` is a frame that draws nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    Skip,
    Drop,
    /// Ask the surface for the mode of this pref, then feed `Picked`.
    PickMode(Pref),
    /// The passes to the new format, then feed `Flipped`.
    Flip {
        on: bool,
    },
    /// The targets anew at the window's current size.
    Rebuild,
    /// A surface, a mode, then feed `Resumed`; a failure feeds `LaneTakes`.
    Resume,
    /// The swapchain and its surface down, then feed `Suspended`.
    Suspend,
}

pub(crate) fn step<F: Copy, M: Copy + PartialEq>(
    s: &Reconfig<F, M>,
    input: Input<F, M>,
) -> (Reconfig<F, M>, Plan) {
    let mut n = *s;
    let lane = s.owner.suspended();
    let plan = match input {
        // No surface to size under the lane: the resume builds at the size recorded here.
        Input::Resize { extent } if lane => {
            n.extent = extent;
            Plan::Skip
        }
        // SDL raises two events per configure; a size the standing swapchain already has asks
        // nothing. What the surface changes on its own still arrives as `OutOfDate`.
        Input::Resize { extent } if s.standing && extent == s.extent => Plan::Skip,
        Input::Resize { .. } => Plan::Rebuild,
        Input::SetPref(p) if p == s.pref => Plan::Skip,
        // No surface to ask under the lane: the resume picks against the one it makes.
        Input::SetPref(p) if lane => {
            n.pref = p;
            Plan::Skip
        }
        // The pref lands with its mode: a failed pick leaves the next call a retry.
        Input::SetPref(p) => Plan::PickMode(p),
        Input::Picked { pref, mode } => {
            n.pref = pref;
            n.present_mode = mode;
            if mode == s.present_mode {
                Plan::Skip
            } else {
                Plan::Rebuild
            }
        }
        // No HDR10 surface: PQ stays on the SDR swapchain and the colour pass tone-maps.
        Input::Follow { pq } => {
            let want = pq && s.hdr10.is_some();
            if want == s.hdr_active {
                Plan::Skip
            } else {
                Plan::Flip { on: want }
            }
        }
        // Minimized, the rebuild keeps the HDR10 swapchain the passes would leave.
        Input::LeaveHdr if !s.hdr_active || lane || s.minimized() => Plan::Skip,
        Input::LeaveHdr => Plan::Flip { on: false },
        Input::Flipped { on, format } => {
            n.format = format;
            n.hdr_active = on;
            Plan::Rebuild
        }
        Input::HdrRefused => {
            n.hdr10 = None;
            Plan::Flip { on: false }
        }
        Input::RealFrame | Input::Redraw { .. } if s.minimized() => Plan::Drop,
        Input::RealFrame
        | Input::Redraw {
            overlay_blocked: true,
        } if lane => {
            n.owner = Owner::Lane {
                shown: false,
                pq: false,
            };
            Plan::Resume
        }
        // It would paint over the lane's picture.
        Input::Redraw { .. } if lane => Plan::Drop,
        Input::RealFrame | Input::Redraw { .. } if !s.standing => Plan::Rebuild,
        Input::RealFrame | Input::Redraw { .. } => Plan::Skip,
        Input::Rebuilt { ok } => {
            n.standing = ok;
            Plan::Skip
        }
        Input::LaneTakes => Plan::Suspend,
        Input::Suspended => {
            n.owner = Owner::Lane {
                shown: false,
                pq: false,
            };
            n.standing = false;
            Plan::Skip
        }
        Input::Resumed { mode } => {
            n.owner = Owner::Swapchain;
            n.present_mode = mode;
            Plan::Rebuild
        }
        Input::Shown { pq } => {
            debug_assert!(lane, "the lane shows only while it owns the window");
            n.owner = Owner::Lane { shown: true, pq };
            Plan::Skip
        }
        Input::OutOfDate | Input::QueueFull if lane => Plan::Skip,
        Input::OutOfDate | Input::QueueFull => Plan::Rebuild,
    };
    (n, plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    type R = Reconfig<&'static str, &'static str>;
    type I = Input<&'static str, &'static str>;

    const PREF: Pref = Pref {
        vsync: true,
        allow_vrr: true,
        fullscreen: false,
    };
    const NO_VSYNC: Pref = Pref {
        vsync: false,
        ..PREF
    };
    const LANE_PQ: Owner = Owner::Lane {
        shown: true,
        pq: true,
    };
    const LANE_FRESH: Owner = Owner::Lane {
        shown: false,
        pq: false,
    };

    /// A windowed SDR swapchain on a surface that offers HDR10.
    fn base() -> R {
        Reconfig {
            owner: Owner::Swapchain,
            hdr_active: false,
            format: "sdr",
            hdr10: Some("hdr10"),
            pref: PREF,
            present_mode: "mailbox",
            extent: (1920, 1080),
            standing: true,
        }
    }

    fn with(edit: impl Fn(&mut R)) -> R {
        let mut r = base();
        edit(&mut r);
        r
    }

    fn hdr(r: &mut R) {
        r.hdr_active = true;
        r.format = "hdr10";
    }

    /// The lane owns the window: no swapchain stands.
    fn on_lane(r: &mut R, owner: Owner) {
        r.owner = owner;
        r.standing = false;
    }

    /// The swapchain owns the window, but its last build failed.
    fn down(r: &mut R) {
        r.standing = false;
    }

    /// (what it pins, from, input, plan, state after)
    type Row = (&'static str, R, I, Plan, R);

    #[test]
    fn every_swapchain_transition_has_one_answer() {
        let table: Vec<Row> = vec![
            (
                "R1-1: a pref change under the lane is kept, no surface asked",
                with(|r| on_lane(r, LANE_PQ)),
                Input::SetPref(NO_VSYNC),
                Plan::Skip,
                with(|r| {
                    on_lane(r, LANE_PQ);
                    r.pref = NO_VSYNC;
                }),
            ),
            (
                "R1-1: leaving HDR under the lane waits for the resume",
                with(|r| {
                    hdr(r);
                    on_lane(r, LANE_PQ);
                }),
                Input::LeaveHdr,
                Plan::Skip,
                with(|r| {
                    hdr(r);
                    on_lane(r, LANE_PQ);
                }),
            ),
            (
                "an unchanged pref asks nothing",
                base(),
                Input::SetPref(PREF),
                Plan::Skip,
                base(),
            ),
            (
                "R1-7: a pref change asks the surface and keeps the old pref",
                base(),
                Input::SetPref(NO_VSYNC),
                Plan::PickMode(NO_VSYNC),
                base(),
            ),
            (
                "R1-7: the pref lands with its mode; a new mode rebuilds",
                base(),
                Input::Picked {
                    pref: NO_VSYNC,
                    mode: "immediate",
                },
                Plan::Rebuild,
                with(|r| {
                    r.pref = NO_VSYNC;
                    r.present_mode = "immediate";
                }),
            ),
            (
                "R1-7: a pick on the same mode rebuilds nothing",
                base(),
                Input::Picked {
                    pref: NO_VSYNC,
                    mode: "mailbox",
                },
                Plan::Skip,
                with(|r| r.pref = NO_VSYNC),
            ),
            (
                "R1-2: leaving HDR while minimized keeps HDR10",
                with(|r| {
                    hdr(r);
                    r.extent = (0, 0);
                }),
                Input::LeaveHdr,
                Plan::Skip,
                with(|r| {
                    hdr(r);
                    r.extent = (0, 0);
                }),
            ),
            (
                "the console leaves HDR10 for SDR",
                with(hdr),
                Input::LeaveHdr,
                Plan::Flip { on: false },
                with(hdr),
            ),
            (
                "leaving HDR on SDR does nothing",
                base(),
                Input::LeaveHdr,
                Plan::Skip,
                base(),
            ),
            (
                "R2-2: a real frame while minimized under the lane drops; the lane keeps it all",
                with(|r| {
                    on_lane(r, LANE_PQ);
                    r.extent = (0, 0);
                }),
                Input::RealFrame,
                Plan::Drop,
                with(|r| {
                    on_lane(r, LANE_PQ);
                    r.extent = (0, 0);
                }),
            ),
            (
                "a frame on a minimized swapchain draws nothing",
                with(|r| r.extent = (0, 0)),
                Input::RealFrame,
                Plan::Drop,
                with(|r| r.extent = (0, 0)),
            ),
            (
                "R2-4: a redraw with an overlay the lane cannot show takes the window back",
                with(|r| on_lane(r, LANE_PQ)),
                Input::Redraw {
                    overlay_blocked: true,
                },
                Plan::Resume,
                with(|r| on_lane(r, LANE_FRESH)),
            ),
            (
                "R2-4: a redraw the lane shows paints nothing over its picture",
                with(|r| on_lane(r, LANE_PQ)),
                Input::Redraw {
                    overlay_blocked: false,
                },
                Plan::Drop,
                with(|r| on_lane(r, LANE_PQ)),
            ),
            (
                "the lane take-over: a real frame resumes; the lane no longer shows",
                with(|r| on_lane(r, LANE_PQ)),
                Input::RealFrame,
                Plan::Resume,
                with(|r| on_lane(r, LANE_FRESH)),
            ),
            (
                "a redraw on the swapchain draws, overlay or not",
                base(),
                Input::Redraw {
                    overlay_blocked: true,
                },
                Plan::Skip,
                base(),
            ),
            (
                "the lane takes the window: the swapchain goes down first",
                base(),
                Input::LaneTakes,
                Plan::Suspend,
                base(),
            ),
            (
                "the swapchain is down: the lane owns, nothing shown yet",
                base(),
                Input::Suspended,
                Plan::Skip,
                with(|r| on_lane(r, LANE_FRESH)),
            ),
            (
                "the lane showed a PQ frame",
                with(|r| on_lane(r, LANE_FRESH)),
                Input::Shown { pq: true },
                Plan::Skip,
                with(|r| on_lane(r, LANE_PQ)),
            ),
            (
                "1e1f4be05: the swapchain owns once the resumed surface answered a mode",
                with(|r| {
                    on_lane(r, LANE_FRESH);
                    r.pref = NO_VSYNC;
                }),
                Input::Resumed { mode: "immediate" },
                Plan::Rebuild,
                with(|r| {
                    r.pref = NO_VSYNC;
                    r.present_mode = "immediate";
                    down(r);
                }),
            ),
            (
                "1e1f4be05: a failed resume hands the window back to the lane",
                with(|r| on_lane(r, LANE_FRESH)),
                Input::LaneTakes,
                Plan::Suspend,
                with(|r| on_lane(r, LANE_FRESH)),
            ),
            (
                "d86625476: a refused HDR10 swapchain costs HDR, not the session",
                with(hdr),
                Input::HdrRefused,
                Plan::Flip { on: false },
                with(|r| {
                    hdr(r);
                    r.hdr10 = None;
                }),
            ),
            (
                "20cce7087: a PQ frame flips on, which picks the overlay's PQ encode",
                base(),
                Input::Follow { pq: true },
                Plan::Flip { on: true },
                base(),
            ),
            (
                "without HDR10 a PQ frame stays on SDR and tone-maps",
                with(|r| r.hdr10 = None),
                Input::Follow { pq: true },
                Plan::Skip,
                with(|r| r.hdr10 = None),
            ),
            (
                "an SDR frame after a PQ stream flips back",
                with(hdr),
                Input::Follow { pq: false },
                Plan::Flip { on: false },
                with(hdr),
            ),
            (
                "a PQ frame on HDR10 flips nothing",
                with(hdr),
                Input::Follow { pq: true },
                Plan::Skip,
                with(hdr),
            ),
            (
                "b4eb42b9f: format and HDR land once the passes stand; the swapchain follows",
                base(),
                Input::Flipped {
                    on: true,
                    format: "hdr10",
                },
                Plan::Rebuild,
                with(hdr),
            ),
            (
                "R1-6: a resize at the standing extent asks nothing",
                base(),
                Input::Resize {
                    extent: (1920, 1080),
                },
                Plan::Skip,
                base(),
            ),
            (
                "a resize to a new size rebuilds",
                base(),
                Input::Resize {
                    extent: (1280, 720),
                },
                Plan::Rebuild,
                base(),
            ),
            (
                "ada44712a: a resize under the lane records the size for the resume",
                with(|r| on_lane(r, LANE_PQ)),
                Input::Resize { extent: (0, 0) },
                Plan::Skip,
                with(|r| {
                    on_lane(r, LANE_PQ);
                    r.extent = (0, 0);
                }),
            ),
            (
                "an out-of-date swapchain rebuilds at the same extent",
                base(),
                Input::OutOfDate,
                Plan::Rebuild,
                base(),
            ),
            (
                "0a46be83c: a full stamp queue rebuilds; the shell disarmed the engine",
                base(),
                Input::QueueFull,
                Plan::Rebuild,
                base(),
            ),
            (
                "out of date under the lane: no swapchain to rebuild",
                with(|r| on_lane(r, LANE_PQ)),
                Input::OutOfDate,
                Plan::Skip,
                with(|r| on_lane(r, LANE_PQ)),
            ),
            (
                "D17: a failed build leaves no swapchain standing",
                base(),
                Input::Rebuilt { ok: false },
                Plan::Skip,
                with(down),
            ),
            (
                "D17: a real frame on no swapchain rebuilds",
                with(down),
                Input::RealFrame,
                Plan::Rebuild,
                with(down),
            ),
            (
                "D17: a redraw on no swapchain rebuilds",
                with(down),
                Input::Redraw {
                    overlay_blocked: false,
                },
                Plan::Rebuild,
                with(down),
            ),
            (
                "D17: out of date on no swapchain rebuilds",
                with(down),
                Input::OutOfDate,
                Plan::Rebuild,
                with(down),
            ),
            (
                "D17: a resize on no swapchain rebuilds",
                with(down),
                Input::Resize {
                    extent: (1280, 720),
                },
                Plan::Rebuild,
                with(down),
            ),
            (
                "D17: the rebuilt targets stand",
                with(down),
                Input::Rebuilt { ok: true },
                Plan::Skip,
                base(),
            ),
            (
                "D17: a frame on no swapchain at a zero extent draws nothing until a resize",
                with(|r| {
                    down(r);
                    r.extent = (0, 0);
                }),
                Input::RealFrame,
                Plan::Drop,
                with(|r| {
                    down(r);
                    r.extent = (0, 0);
                }),
            ),
            (
                "D17: a pref change on no swapchain asks the surface",
                with(down),
                Input::SetPref(NO_VSYNC),
                Plan::PickMode(NO_VSYNC),
                with(down),
            ),
            (
                "D17: a PQ frame on no swapchain flips; the flip's rebuild stands it",
                with(down),
                Input::Follow { pq: true },
                Plan::Flip { on: true },
                with(down),
            ),
            (
                "D17: the console leaves HDR10 on no swapchain",
                with(|r| {
                    hdr(r);
                    down(r);
                }),
                Input::LeaveHdr,
                Plan::Flip { on: false },
                with(|r| {
                    hdr(r);
                    down(r);
                }),
            ),
            (
                "D17: no build runs under the lane; a failure fed there changes nothing",
                with(|r| on_lane(r, LANE_PQ)),
                Input::Rebuilt { ok: false },
                Plan::Skip,
                with(|r| on_lane(r, LANE_PQ)),
            ),
        ];
        for (name, from, input, plan, after) in table {
            let (got, got_plan) = step(&from, input);
            assert_eq!(got_plan, plan, "{name}");
            assert_eq!(got, after, "{name}");
        }
    }

    const REAL: I = Input::RealFrame;
    const REDRAW: I = Input::Redraw {
        overlay_blocked: false,
    };
    const BUILT: I = Input::Rebuilt { ok: true };

    /// (inputs, plans, suspended, native_last, native_pq)
    type OwnerRow = (&'static [I], &'static [Plan], bool, bool, bool);

    #[test]
    fn the_window_changes_hands_only_on_a_real_frame() {
        use Input::{LaneTakes, Rebuilt, Resumed, Shown, Suspended};
        use Plan::{Drop, Rebuild, Resume, Skip, Suspend};
        let table: &[OwnerRow] = &[
            (&[REAL, REDRAW], &[Skip, Skip], false, false, false),
            // The lane took the window and showed a frame: a redraw paints nothing.
            (
                &[LaneTakes, Suspended, Shown { pq: true }, REDRAW],
                &[Suspend, Skip, Skip, Drop],
                true,
                true,
                true,
            ),
            // Suspended without a frame shown (the commit dropped): still the lane's.
            (
                &[LaneTakes, Suspended, REDRAW],
                &[Suspend, Skip, Drop],
                true,
                false,
                false,
            ),
            // A real frame takes the window back; the lane no longer shows.
            (
                &[Suspended, Shown { pq: true }, REAL],
                &[Skip, Skip, Resume],
                true,
                false,
                false,
            ),
            // A resume that did not stand leaves the lane's state: the next frame resumes again.
            (
                &[Suspended, Shown { pq: false }, REAL, REAL],
                &[Skip, Skip, Resume, Resume],
                true,
                false,
                false,
            ),
            (
                &[
                    Suspended,
                    Shown { pq: false },
                    REAL,
                    Resumed { mode: "fifo" },
                    BUILT,
                    REDRAW,
                ],
                &[Skip, Skip, Resume, Rebuild, Skip, Skip],
                false,
                false,
                false,
            ),
            // D17: a failed build; the next frame rebuilds, the one after draws.
            (
                &[Rebuilt { ok: false }, REAL, BUILT, REAL],
                &[Skip, Rebuild, Skip, Skip],
                false,
                false,
                false,
            ),
        ];
        for (inputs, plans, suspended, native_last, native_pq) in table {
            let mut s = base();
            let mut got = Vec::new();
            for &input in *inputs {
                let (next, plan) = step(&s, input);
                s = next;
                got.push(plan);
            }
            assert_eq!(&got, plans, "{inputs:?}");
            assert_eq!(
                s.owner.suspended(),
                *suspended,
                "suspended after {inputs:?}"
            );
            assert_eq!(
                s.owner.native_last(),
                *native_last,
                "native_last after {inputs:?}"
            );
            assert_eq!(
                s.owner.native_pq(),
                *native_pq,
                "native_pq after {inputs:?}"
            );
        }
    }
}
