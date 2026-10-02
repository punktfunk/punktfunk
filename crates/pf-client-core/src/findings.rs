//! A network check's finding in words, from its id and figures
//! ([`punktfunk_core::client::health::FindingId`] as a byte). Every shell shows the same
//! sentence: what did not happen, then the next move. The offered profile is the shell's
//! button, not a sentence here.

/// The sentence for finding `id` with its three figures.
pub fn text(id: u8, numbers: [u32; 3]) -> String {
    let [a, b, c] = numbers;
    let pct = |x: u32| f64::from(x) / 100.0;
    match id {
        1 => {
            if a > 0 && b > 0 {
                format!(
                    "The host's port is faster than this device's ({a} vs {b} Mbit/s), so \
                     bursts overflow the switch between them."
                )
            } else {
                "The host's port is faster than this device's, so bursts overflow the \
                 switch between them."
                    .to_string()
            }
        }
        2 => format!(
            "This device drops the start of every burst ({:.1} % lost) \u{2014} the \
             adapter's power saving is the usual cause.",
            pct(a)
        ),
        3 => {
            if a > 0 {
                format!(
                    "This device's own receive buffer dropped {a} packets; the system caps it \
                     at {b} KB."
                )
            } else {
                format!("The system caps this device's receive buffer at {b} KB.")
            }
        }
        4 => format!(
            "Loss at a rate no link refuses ({:.1} %): check the cable, the port or the \
             adapter driver.",
            pct(a)
        ),
        5 => format!(
            "Something on the path buffers instead of dropping ({:.0} ms spread); keep the \
             bitrate under {:.0} Mbit/s.",
            f64::from(a) / 1000.0,
            f64::from(b) / 1000.0
        ),
        6 => {
            if a > 0 {
                format!("The host's send buffer refused {a} packets; raise its limit.")
            } else {
                format!("The host's send buffer is capped at {b} KB; raise its limit.")
            }
        }
        7 => {
            if a > 0 {
                format!("This device is on Wi-Fi; bursts lose {:.1} %.", pct(a))
            } else {
                "This device is on Wi-Fi.".to_string()
            }
        }
        _ => format!("Finding {id} ({a}, {b}, {c})."),
    }
}

/// What the offered profile is called on a button: `1` capped, `2` smooth.
pub fn profile_name(profile: u8) -> &'static str {
    match profile {
        1 => "capped",
        2 => "smooth",
        _ => "none",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every id has a sentence that ends with a full stop and names its figure where it
    /// has one; an unknown id still says something.
    #[test]
    fn every_finding_has_a_sentence() {
        for id in 1..=7u8 {
            let s = text(id, [2500, 1000, 150]);
            assert!(s.ends_with('.'), "{id}: {s}");
            assert!(!s.contains("Finding"), "{id} is known: {s}");
        }
        assert!(text(1, [2500, 1000, 0]).contains("2500 vs 1000"));
        assert!(text(2, [150, 0, 0]).contains("1.5 %"));
        assert!(text(3, [40, 208, 0]).contains("40 packets"));
        assert!(text(7, [0, 0, 0]).ends_with("Wi-Fi."));
        assert!(text(9, [1, 2, 3]).starts_with("Finding 9"));
        assert_eq!(profile_name(1), "capped");
    }
}
