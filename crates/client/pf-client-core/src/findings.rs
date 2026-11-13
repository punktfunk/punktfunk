//! A network check's finding in words, from its id and figures
//! ([`punktfunk_core::client::health::FindingId`] as a byte). Every shell shows the same
//! sentence: what did not happen, then the next move. `clients/shared/finding-vectors.json`
//! pins the message, its arguments and the English sentence for the Kotlin and Swift copies.

use std::fmt::Display;

/// Finding `id` with its three figures as a message: its key, and its arguments by placeholder
/// name. Each figure is already formatted, `.` separated and rounded half to even, the same in
/// every language. An id this build doesn't know is `finding_unknown_body`.
pub fn message(id: u8, numbers: [u32; 3]) -> (&'static str, Vec<(&'static str, String)>) {
    let [a, b, _] = numbers;
    let pct = |x: u32| format!("{:.1}", f64::from(x) / 100.0);
    match id {
        1 if a > 0 && b > 0 => (
            "finding_speed_mismatch_body",
            vec![("host_mbit", a.to_string()), ("device_mbit", b.to_string())],
        ),
        1 => ("finding_speed_mismatch_unknown_body", vec![]),
        2 => ("finding_burst_intolerant_body", vec![("loss", pct(a))]),
        3 if a > 0 => (
            "finding_receive_buffer_body",
            vec![("packets", a.to_string()), ("cap_kb", b.to_string())],
        ),
        3 => (
            "finding_receive_buffer_cap_body",
            vec![("cap_kb", b.to_string())],
        ),
        4 => ("finding_link_fault_body", vec![("loss", pct(a))]),
        5 => (
            "finding_queue_build_up_body",
            vec![
                ("spread_ms", format!("{:.0}", f64::from(a) / 1000.0)),
                ("max_mbit", format!("{:.0}", f64::from(b) / 1000.0)),
            ],
        ),
        6 if a > 0 => (
            "finding_host_send_buffer_body",
            vec![("packets", a.to_string())],
        ),
        6 => (
            "finding_host_send_buffer_cap_body",
            vec![("cap_kb", b.to_string())],
        ),
        7 if a > 0 => ("finding_wifi_loss_body", vec![("loss", pct(a))]),
        7 => ("finding_wifi_body", vec![]),
        _ => ("finding_unknown_body", vec![("id", id.to_string())]),
    }
}

/// The sentence for finding `id` with its three figures, in the language set.
pub fn text(id: u8, numbers: [u32; 3]) -> String {
    let (key, args) = message(id, numbers);
    let args: Vec<(&str, &dyn Display)> =
        args.iter().map(|(k, v)| (*k, v as &dyn Display)).collect();
    // Both plurals count `packets`, the first figure; a plain message ignores the count.
    pf_l10n::get(key).map_or_else(String::new, |m| m.format_count(numbers[0].into(), &args))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared vectors: every known id, both branches where a figure can be zero, the
    /// rounding ties, and an unknown id. Each case's message and arguments, then its English.
    #[test]
    fn shared_vectors() {
        let raw = include_str!("../../../../clients/shared/finding-vectors.json");
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
            let numbers = [n[0], n[1], n[2]];
            let (key, args) = message(id, numbers);
            assert_eq!(key, case["key"].as_str().unwrap(), "{case}");
            let mut want: Vec<(String, String)> = case["args"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
                .collect();
            let mut got: Vec<(String, String)> =
                args.into_iter().map(|(k, v)| (k.into(), v)).collect();
            got.sort();
            want.sort();
            assert_eq!(got, want, "{case}");
            assert_eq!(text(id, numbers), case["sentence"].as_str().unwrap());
        }
        let ids: Vec<u64> = cases.iter().map(|c| c["id"].as_u64().unwrap()).collect();
        assert!(
            (1..=7).all(|id| ids.contains(&id)),
            "every known id has a row"
        );
        assert!(ids.iter().any(|&id| id > 7), "an unknown id has a row");
    }
}
