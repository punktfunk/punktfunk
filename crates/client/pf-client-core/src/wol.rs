//! Client Wake-on-LAN: parse stored MAC strings and send via `punktfunk_core::wol`.
//! A sleeping host has no ARP entry; the core's broadcast is what actually wakes it.

use std::net::Ipv4Addr;

/// Typed MAC text as the list the host store keeps. Entries split on commas, semicolons or
/// whitespace; each is six hex pairs joined by `:` or `-`, or twelve bare hex digits. Stored
/// lower-case with `:`, in order, without repeats. `Err` carries the first entry that isn't one.
pub fn parse_mac_list(text: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for token in text
        .split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .filter(|t| !t.is_empty())
    {
        let bare = token.len() == 12 && token.bytes().all(|b| b.is_ascii_hexdigit());
        let spelled = if bare {
            token
                .as_bytes()
                .chunks(2)
                .map(|p| std::str::from_utf8(p).unwrap_or_default())
                .collect::<Vec<_>>()
                .join(":")
        } else {
            token.to_string()
        };
        let pairs_ok = spelled.split([':', '-']).all(|p| p.len() == 2);
        let mac = punktfunk_core::wol::parse_mac(&spelled)
            .filter(|_| pairs_ok)
            .ok_or_else(|| token.to_string())?;
        let stored = mac
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":");
        if !out.contains(&stored) {
            out.push(stored);
        }
    }
    Ok(out)
}

pub fn wake(macs: &[String], last_ip: Option<Ipv4Addr>) {
    let parsed: Vec<[u8; 6]> = macs
        .iter()
        .filter_map(|s| punktfunk_core::wol::parse_mac(s))
        .collect();
    if parsed.is_empty() {
        tracing::warn!("wake requested but no valid MAC is known for this host");
        return;
    }
    match punktfunk_core::wol::send_magic_packet(&parsed, last_ip) {
        Ok(()) => tracing::info!(count = parsed.len(), "sent Wake-on-LAN magic packet"),
        Err(e) => tracing::warn!(error = %e, "Wake-on-LAN send failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_mac_list;

    #[test]
    fn typed_macs_store_in_one_spelling() {
        assert_eq!(
            parse_mac_list("AA-BB-CC-DD-EE-FF, 01:02:03:04:05:06"),
            Ok(vec![
                "aa:bb:cc:dd:ee:ff".to_string(),
                "01:02:03:04:05:06".to_string()
            ])
        );
        assert_eq!(
            parse_mac_list("aabbccddeeff aa:bb:cc:dd:ee:ff;"),
            Ok(vec!["aa:bb:cc:dd:ee:ff".to_string()])
        );
        assert_eq!(parse_mac_list("  "), Ok(Vec::new()));
    }

    #[test]
    fn the_first_entry_that_is_not_a_mac_is_named() {
        let err = |s: &str| Err(s.to_string());
        assert_eq!(
            parse_mac_list("aa:bb:cc:dd:ee:ff, aa:bb:cc"),
            err("aa:bb:cc")
        );
        assert_eq!(parse_mac_list("a:b:c:d:e:f"), err("a:b:c:d:e:f"));
        assert_eq!(
            parse_mac_list("aa:bb:cc:dd:ee:0ff"),
            err("aa:bb:cc:dd:ee:0ff")
        );
        assert_eq!(
            parse_mac_list("zz:bb:cc:dd:ee:ff"),
            err("zz:bb:cc:dd:ee:ff")
        );
    }
}
