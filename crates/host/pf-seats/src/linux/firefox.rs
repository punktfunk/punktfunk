//! The box's web console in every account's Firefox: an enterprise policy installs the
//! certificate the door's console serves, so a seat's "open my console" link loads with no
//! warning. Firefox reads no system store for that certificate, a leaf with no CA bit. Flathub's
//! Firefox reads its policies through its `systemconfig` extension at `/app/etc/firefox`, a
//! packaged one from `/etc/firefox`.
//!
//! The door's account writes the certificate, so root installs only one that vouches for this
//! box alone ([`checked`]), and writes the DER it checked, never the door's file.

use super::safe_open;
use super::session::write_if_changed;
use std::io::{Error, ErrorKind};
use std::path::{Path, PathBuf};

const CERT: &str = "punktfunk-console.der";

/// Each `policies` directory a Firefox on this box reads, and that directory as it sees it.
fn targets() -> Vec<(PathBuf, &'static str)> {
    let mut out = Vec::new();
    if Path::new("/var/lib/flatpak").is_dir() {
        let arch = std::env::consts::ARCH;
        let ext =
            format!("/var/lib/flatpak/extension/org.mozilla.firefox.systemconfig/{arch}/stable");
        out.push((
            Path::new(&ext).join("policies"),
            "/app/etc/firefox/policies",
        ));
    }
    // The binary, not the folder: Fedora's OpenH264 plugin makes `/usr/lib64/firefox` alone.
    let packaged = ["/usr/lib/firefox/firefox", "/usr/lib64/firefox/firefox"];
    if packaged.iter().any(|p| Path::new(p).is_file()) {
        out.push(("/etc/firefox/policies".into(), "/etc/firefox/policies"));
    }
    out
}

/// Installs the certificate of the console in `box_dir` for every Firefox on the box. A door that
/// has none yet changes nothing; one [`checked`] refuses is an error.
pub(super) fn level(box_dir: &Path) -> std::io::Result<()> {
    let Some((pem, _)) = safe_open::read(&box_dir.join("native-cert.pem"), 64 * 1024)? else {
        return Ok(());
    };
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    let der = checked(&pem, hostname.trim()).ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidData,
            "the console's certificate vouches for more than this box",
        )
    })?;
    for (dir, seen) in targets() {
        write_if_changed(&dir.join(CERT), &der, 0o644)?;
        let path = dir.join("policies.json");
        let current = match std::fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        if let Some(next) = with_cert(current.as_deref(), &format!("{seen}/{CERT}"))? {
            write_if_changed(&path, next, 0o644)?;
        }
    }
    Ok(())
}

/// The DER of `pem` when it vouches for this box alone: a v3 certificate with no CA bit, so it
/// signs no other, whose every name is `localhost`, `127.0.0.1`, `::1` or `hostname`.
fn checked(pem: &[u8], hostname: &str) -> Option<Vec<u8>> {
    use x509_parser::extensions::{GeneralName, ParsedExtension};
    use x509_parser::oid_registry::OID_X509_EXT_BASIC_CONSTRAINTS;
    let (_, pem) = x509_parser::pem::parse_x509_pem(pem).ok()?;
    let (rest, cert) = x509_parser::parse_x509_certificate(&pem.contents).ok()?;
    let v3 = cert.version() == x509_parser::x509::X509Version::V3;
    let ca = match cert.get_extension_unique(&OID_X509_EXT_BASIC_CONSTRAINTS) {
        Ok(None) => false,
        Ok(Some(ext)) => {
            !matches!(ext.parsed_extension(), ParsedExtension::BasicConstraints(bc) if !bc.ca)
        }
        Err(_) => true,
    };
    let names = cert
        .subject_alternative_name()
        .ok()??
        .value
        .general_names
        .clone();
    let ours = |name: &GeneralName| match name {
        GeneralName::DNSName(dns) => {
            dns.eq_ignore_ascii_case("localhost")
                || (!hostname.is_empty() && dns.eq_ignore_ascii_case(hostname))
        }
        GeneralName::IPAddress(ip) => {
            ip[..] == [127, 0, 0, 1] || ip[..] == std::net::Ipv6Addr::LOCALHOST.octets()
        }
        _ => false,
    };
    let fits = pem.label == "CERTIFICATE" && rest.is_empty() && v3 && !ca;
    (fits && !names.is_empty() && names.iter().all(ours)).then(|| pem.contents.clone())
}

/// `policies` (`None`: no file yet) with `path` among the certificates Firefox installs, or `None`
/// when it lists it already. Another policy stays as it is; a file that isn't a policy object is
/// refused.
fn with_cert(policies: Option<&str>, path: &str) -> std::io::Result<Option<String>> {
    let mut root: serde_json::Value = match policies {
        Some(text) => serde_json::from_str(text)?,
        None => serde_json::json!({}),
    };
    let foreign = || {
        Error::new(
            ErrorKind::InvalidData,
            "policies.json holds no policy object",
        )
    };
    let mut at = &mut root;
    for key in ["policies", "Certificates"] {
        let map = at.as_object_mut().ok_or_else(foreign)?;
        at = map.entry(key).or_insert_with(|| serde_json::json!({}));
    }
    let map = at.as_object_mut().ok_or_else(foreign)?;
    let list = map
        .entry("Install")
        .or_insert_with(|| serde_json::json!([]));
    let list = list.as_array_mut().ok_or_else(foreign)?;
    if list.iter().any(|p| p == path) {
        return Ok(None);
    }
    list.push(path.into());
    Ok(Some(serde_json::to_string_pretty(&root)? + "\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A self-signed leaf for `names`, minted as the host mints its own; a CA when `ca`.
    fn pem(names: &[&str], ca: bool) -> String {
        let names = names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        let mut params = rcgen::CertificateParams::new(names).unwrap();
        if ca {
            params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        }
        params
            .self_signed(&rcgen::KeyPair::generate().unwrap())
            .unwrap()
            .pem()
    }

    #[test]
    fn only_a_certificate_for_this_box_alone_is_installed() {
        let box_names = ["bazzite", "localhost", "127.0.0.1", "::1"];
        let own = pem(&box_names, false);
        let der = checked(own.as_bytes(), "bazzite").expect("the host's own shape");
        let (_, parsed) = x509_parser::pem::parse_x509_pem(own.as_bytes()).unwrap();
        assert_eq!(
            der, parsed.contents,
            "the checked DER, not the door's bytes"
        );
        assert!(
            checked(own.as_bytes(), "BAZZITE").is_some(),
            "names match without case"
        );

        assert!(
            checked(own.as_bytes(), "other").is_none(),
            "a hostname the box doesn't have"
        );
        assert!(
            checked(pem(&box_names, true).as_bytes(), "bazzite").is_none(),
            "a CA"
        );
        let wider = pem(&["localhost", "example.com"], false);
        assert!(
            checked(wider.as_bytes(), "bazzite").is_none(),
            "another site's name"
        );
        assert!(checked(pem(&["10.0.0.5"], false).as_bytes(), "bazzite").is_none());
        assert!(
            checked(pem(&[], false).as_bytes(), "bazzite").is_none(),
            "no names"
        );
        assert!(checked(b"not a certificate", "bazzite").is_none());
        assert!(
            checked(pem(&["localhost"], false).as_bytes(), "").is_some(),
            "no hostname"
        );
    }

    #[test]
    fn the_policy_joins_what_is_there() {
        let path = "/app/etc/firefox/policies/punktfunk-console.der";
        let fresh = with_cert(None, path).unwrap().unwrap();
        let v: serde_json::Value = serde_json::from_str(&fresh).unwrap();
        assert_eq!(
            v["policies"]["Certificates"]["Install"],
            serde_json::json!([path])
        );
        assert_eq!(with_cert(Some(&fresh), path).unwrap(), None, "listed once");

        let admin =
            r#"{"policies":{"DisableTelemetry":true,"Certificates":{"Install":["corp.pem"]}}}"#;
        let v: serde_json::Value =
            serde_json::from_str(&with_cert(Some(admin), path).unwrap().unwrap()).unwrap();
        assert_eq!(v["policies"]["DisableTelemetry"], true);
        assert_eq!(
            v["policies"]["Certificates"]["Install"],
            serde_json::json!(["corp.pem", path])
        );

        assert!(with_cert(Some("[]"), path).is_err());
        assert!(with_cert(Some(r#"{"policies":[]}"#), path).is_err());
        assert!(with_cert(Some("{ not json"), path).is_err());
    }
}
