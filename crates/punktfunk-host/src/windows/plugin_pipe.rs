//! A named-pipe instance with the plugin pipe's DACL. The one unsafe step of `mgmt::pipes`,
//! which is `forbid(unsafe_code)`: the SDDL becomes a descriptor, the instance copies it at
//! create time, and the descriptor is freed before this returns.

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

/// SYSTEM and Administrators in full; LocalService — the runner and, today, every plugin
/// process — may open, read and write. `P`: nothing inherited. Never `ALL APPLICATION
/// PACKAGES`: a package gets its own ACE, on its own pipe.
const PIPE_SDDL: PCWSTR = w!("D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFW;;;LS)");

/// One instance of `name` under [`PIPE_SDDL`]. `first` claims the name: a second server on
/// it, from any account, fails instead of sharing the plugin's traffic. Remote clients are
/// refused by the pipe itself.
pub(crate) fn create_plugin_pipe(name: &str, first: bool) -> std::io::Result<NamedPipeServer> {
    let mut psd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: the SDDL literal is valid; `psd` receives a `LocalAlloc`'d descriptor, freed below
    // once the create that borrows it has returned.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PIPE_SDDL,
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
