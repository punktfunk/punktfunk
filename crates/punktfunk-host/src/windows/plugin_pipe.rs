//! A named-pipe instance with the plugin pipe's DACL. The one unsafe step of `mgmt::pipes`,
//! which is `forbid(unsafe_code)`: the SDDL becomes a descriptor, the instance copies it at
//! create time, and the descriptor is freed before this returns.

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

/// SYSTEM and Administrators in full; LocalService — the runner, and the user half of a
/// container's check — may open, read and write, as may the plugin's own package when it has
/// one. `P`: nothing inherited. Never `ALL APPLICATION PACKAGES`.
fn pipe_sddl(package_sid: Option<&str>) -> String {
    let package = package_sid
        .map(|sid| format!("(A;;FRFW;;;{sid})"))
        .unwrap_or_default();
    format!("D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFW;;;LS){package}")
}

/// One instance of `name` under [`pipe_sddl`]. `first` claims the name: a second server on
/// it, from any account, fails instead of sharing the plugin's traffic. Remote clients are
/// refused by the pipe itself.
pub(crate) fn create_plugin_pipe(
    name: &str,
    first: bool,
    package_sid: Option<&str>,
) -> std::io::Result<NamedPipeServer> {
    let sddl: Vec<u16> = pipe_sddl(package_sid)
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut psd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `sddl` is NUL-terminated and well-formed; `psd` receives a `LocalAlloc`'d
    // descriptor, freed below once the create that borrows it has returned.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut psd,
            None,
        )
        .map_err(std::io::Error::other)?;
    }
    let mut sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: psd.0,
        bInheritHandle: false.into(),
    };
    // SAFETY: `sa` is a complete `SECURITY_ATTRIBUTES` whose descriptor is live until the
    // `LocalFree` below, after the call has returned.
    let created = unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(name, (&mut sa as *mut SECURITY_ATTRIBUTES).cast())
    };
    // SAFETY: `psd` came from `ConvertStringSecurityDescriptorToSecurityDescriptorW`; the
    // matching free, after its last borrower returned.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(psd.0)));
    }
    created
}

#[cfg(test)]
mod tests {
    /// A package gets its own ACE after the fixed three; none means the three alone.
    #[test]
    fn the_dacl_names_the_package_when_there_is_one() {
        assert_eq!(
            super::pipe_sddl(None),
            "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFW;;;LS)"
        );
        assert_eq!(
            super::pipe_sddl(Some("S-1-15-2-1-2-3")),
            "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFW;;;LS)(A;;FRFW;;;S-1-15-2-1-2-3)"
        );
    }
}
