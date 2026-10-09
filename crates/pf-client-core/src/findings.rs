//! A network check's finding in words, from its id and figures
//! ([`punktfunk_core::client::health::FindingId`] as a byte). Every shell shows the same
//! sentence: what did not happen, then the next move. `clients/shared/finding-vectors.json`
//! pins the wording and the figures for the Kotlin and Swift copies.

/// The sentence for finding `id` with its three figures. An id this build doesn't know
/// reads `Finding {id}.`; its figures carry no unit to show them with.
pub fn text(id: u8, numbers: [u32; 3]) -> String {
    let [a, b, _] = numbers;
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
        _ => format!("Finding {id}."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared vectors: every known id, both branches where a figure can be zero, the
    /// rounding ties, and an unknown id.
    #[test]
    fn shared_vectors() {
        let raw = include_str!("../../../clients/shared/finding-vectors.json");
        let file: serde_json::Value = serde_json::from_str(raw).expect("vector file parses");
        let cases = file["cases"].as_array().expect("cases array");
        for case in cases {
            let n: Vec<u32> = case["numbers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap() as u32)
                .collect();
            let id = case["id"].as_u64().unwrap() as u8;
            assert_eq!(
                text(id, [n[0], n[1], n[2]]),
                case["sentence"].as_str().unwrap()
            );
        }
        let ids: Vec<u64> = cases.iter().map(|c| c["id"].as_u64().unwrap()).collect();
        assert!(
            (1..=7).all(|id| ids.contains(&id)),
            "every known id has a row"
        );
        assert!(ids.iter().any(|&id| id > 7), "an unknown id has a row");
    }
}
