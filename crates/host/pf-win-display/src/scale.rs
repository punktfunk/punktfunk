//! A display's desktop scale, as Windows counts it.
//!
//! Windows offers the fixed list of scales in [`SCALES`] and addresses one by its distance, in
//! list entries, from the scale it recommends for that display. It files a scale set by hand
//! under the display's exact EDID, so a display whose EDID changed starts at the recommended
//! scale again. A host that wants a scale to outlive that reads and writes it here.
//!
//! The arithmetic is pure and tested everywhere; the two OS calls are Windows-only.

/// Every scale Windows offers, in percent.
const SCALES: [u32; 12] = [100, 125, 150, 175, 200, 225, 250, 300, 350, 400, 450, 500];

/// One display's scale: entries of [`SCALES`] counted from the recommended scale, which is 0.
/// `min` is zero or negative and `max` zero or positive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Steps {
    pub min: i32,
    pub cur: i32,
    pub max: i32,
}

impl Steps {
    /// The scale `step` entries from the recommended one, in percent. The recommended scale
    /// is entry `-min` of [`SCALES`].
    fn percent_at(self, step: i32) -> Option<u32> {
        let index = usize::try_from(step.checked_sub(self.min)?).ok()?;
        SCALES.get(index).copied()
    }

    /// The scale the display runs at, in percent. `None` while Windows' custom scaling is on:
    /// it puts one scale on every display and reports a step outside `min..=max`.
    pub fn percent(self) -> Option<u32> {
        (self.min..=self.max)
            .contains(&self.cur)
            .then(|| self.percent_at(self.cur))
            .flatten()
    }

    /// The step whose scale is nearest `percent`, among those the display offers.
    pub fn nearest(self, percent: u32) -> Option<i32> {
        (self.min..=self.max)
            .filter_map(|step| Some((step, self.percent_at(step)?)))
            .min_by_key(|&(_, offered)| offered.abs_diff(percent))
            .map(|(step, _)| step)
    }

    /// What a host does with a display that just arrived, when it remembers `remembered`
    /// percent for that display. A remembered scale wins. With none, a scale away from the
    /// recommended one was set by hand and is worth remembering.
    pub fn on_arrival(self, remembered: Option<u32>) -> Arrival {
        match remembered {
            Some(percent) => match self.nearest(percent) {
                Some(step) if step != self.cur => Arrival::MoveTo(step),
                _ => Arrival::Nothing,
            },
            None => match self.percent() {
                Some(percent) if self.cur != 0 => Arrival::Remember(percent),
                _ => Arrival::Nothing,
            },
        }
    }
}

/// What [`Steps::on_arrival`] decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrival {
    /// Move the display to this step.
    MoveTo(i32),
    /// Remember this scale, in percent.
    Remember(u32),
    Nothing,
}

#[cfg(target_os = "windows")]
pub use imp::{read, write};

#[cfg(target_os = "windows")]
mod imp {
    use super::Steps;
    use crate::snapshot::CcdTargetKey;
    use crate::{ccd_info, win_display};

    /// The scale of the display on `key`. `None` when the target is not an active path or the
    /// OS does not answer.
    pub fn read(key: CcdTargetKey) -> Option<Steps> {
        let p = win_display::active_path(key)?;
        let (min, cur, max) = ccd_info::dpi_scale_steps(p.sourceInfo.adapterId, p.sourceInfo.id)?;
        Some(Steps { min, cur, max })
    }

    /// Put the display on `key` at `step` from its recommended scale. True when the OS took it.
    pub fn write(key: CcdTargetKey, step: i32) -> bool {
        let Some(p) = win_display::active_path(key) else {
            return false;
        };
        let rc = ccd_info::set_dpi_scale_step(p.sourceInfo.adapterId, p.sourceInfo.id, step);
        tracing::debug!(target = %key, step, rc, "virtual-display scale write");
        rc == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4K display Windows recommends 300 % for: seven entries below it, four above.
    const TV: Steps = Steps {
        min: -7,
        cur: -5,
        max: 4,
    };

    #[test]
    fn a_step_reads_as_the_scale_that_far_from_the_recommended_one() {
        assert_eq!(TV.percent(), Some(150));
        assert_eq!(Steps { cur: 0, ..TV }.percent(), Some(300));
        let small = Steps {
            min: 0,
            cur: 1,
            max: 3,
        };
        assert_eq!(small.percent(), Some(125));
    }

    #[test]
    fn a_step_outside_the_range_is_custom_scaling() {
        assert_eq!(Steps { cur: 5, ..TV }.percent(), None);
        assert_eq!(Steps { cur: -8, ..TV }.percent(), None);
        assert_eq!(
            Steps {
                cur: i32::MIN,
                ..TV
            }
            .percent(),
            None
        );
    }

    #[test]
    fn a_scale_maps_to_the_nearest_step_the_display_offers() {
        assert_eq!(TV.nearest(150), Some(-5));
        assert_eq!(TV.nearest(300), Some(0));
        // 133 % is not on the list: 125 % is nearer than 150 %.
        assert_eq!(TV.nearest(133), Some(-6));
        // A display that tops out at 125 % cannot run 150 %.
        let small = Steps {
            min: 0,
            cur: 0,
            max: 1,
        };
        assert_eq!(small.nearest(150), Some(1));
        assert_eq!(small.nearest(50), Some(0));
    }

    #[test]
    fn an_arriving_display_takes_the_remembered_scale_or_teaches_its_own() {
        // Remembered 200 %, running 150 %: three entries up from the recommended 300 % is -3.
        assert_eq!(TV.on_arrival(Some(200)), Arrival::MoveTo(-3));
        assert_eq!(TV.on_arrival(Some(150)), Arrival::Nothing);
        // Nothing remembered: 150 % on a display recommended at 300 % was set by hand.
        assert_eq!(TV.on_arrival(None), Arrival::Remember(150));
        assert_eq!(Steps { cur: 0, ..TV }.on_arrival(None), Arrival::Nothing);
        // Custom scaling: no scale of its own to learn.
        assert_eq!(Steps { cur: 9, ..TV }.on_arrival(None), Arrival::Nothing);
    }

    #[test]
    fn a_range_past_the_list_offers_only_what_the_list_holds() {
        // `max` claims six entries above 300 %; the list has four.
        let past = Steps {
            min: -7,
            cur: 0,
            max: 6,
        };
        assert_eq!(past.nearest(900), Some(4));
        assert_eq!(Steps { cur: 6, ..past }.percent(), None);
    }
}
