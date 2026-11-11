//! `punktfunk-seats loopback <account>`: a made seat's own loopback address for Steam's UI
//! link. The seat unit runs it as root on every start (`ExecStartPost=`).
//!
//! Steam's client serves its UI on `127.0.0.1:57343`, an address fixed in `steamclient.so`, and
//! every user on a box shares one loopback. The first Steam to start takes the port; every
//! other user's Steam UI then connects to that one, gets none of its own controller input and
//! goes down with it. Two cgroup socket-address programs on the seat user's slice send a bind
//! or a connect to that address to one of the seat's own ([`address`]). Nothing outside the
//! slice changes, the owner's Steam included.
//!
//! The programs belong to the slice's cgroup and go with it, so every start attaches them, once
//! the session it opened exists: that session's slice is the one that stays. A row on a
//! person's own account gets none, its slice is their login.

use std::net::Ipv4Addr;

/// The verb of `punktfunk-seats` that runs the helper.
pub const VERB: &str = "loopback";

/// Steam's UI link.
const STEAM_UI: (Ipv4Addr, u16) = (Ipv4Addr::LOCALHOST, 57343);

// `include/uapi/linux/bpf.h`.
const LDX_W: u8 = 0x61;
const STX_W: u8 = 0x63;
const JNE_K: u8 = 0x55;
const MOV_K: u8 = 0xb7;
const EXIT: u8 = 0x95;

/// `uid`'s own loopback address, in `127.128.0.0/9`: the uid's low 23 bits, so never
/// `127.0.0.1` and never another seat's.
fn address(uid: u32) -> Ipv4Addr {
    Ipv4Addr::new(
        127,
        128 | ((uid >> 16) as u8 & 127),
        (uid >> 8) as u8,
        uid as u8,
    )
}

/// The program of both hooks, over `struct bpf_sock_addr`: a bind or a connect to [`STEAM_UI`]
/// goes to `to`, anything else passes as it is. Both fields hold network byte order.
fn program(to: Ipv4Addr) -> Vec<u8> {
    const USER_IP4: i16 = 4;
    const USER_PORT: i16 = 24;
    let ip = |addr: Ipv4Addr| u32::from_ne_bytes(addr.octets());
    [
        insn(LDX_W, 2, 1, USER_IP4, 0),
        insn(JNE_K, 2, 0, 4, ip(STEAM_UI.0)),
        insn(LDX_W, 2, 1, USER_PORT, 0),
        insn(JNE_K, 2, 0, 2, u32::from(STEAM_UI.1.to_be())),
        insn(MOV_K, 2, 0, 0, ip(to)),
        insn(STX_W, 1, 2, USER_IP4, 0),
        // 1 lets the call through.
        insn(MOV_K, 0, 0, 0, 1),
        insn(EXIT, 0, 0, 0, 0),
    ]
    .concat()
}

/// One `struct bpf_insn`.
fn insn(code: u8, dst: u8, src: u8, off: i16, imm: u32) -> [u8; 8] {
    let (off, imm) = (off.to_ne_bytes(), imm.to_ne_bytes());
    [
        code,
        src << 4 | dst,
        off[0],
        off[1],
        imm[0],
        imm[1],
        imm[2],
        imm[3],
    ]
}

/// The helper. The unit ignores its exit code: a kernel that refuses the hook leaves a seat
/// that starts all the same.
#[cfg(target_os = "linux")]
pub fn run(account: &str) -> u8 {
    match linux::attach(account) {
        Ok(Some(address)) => {
            eprintln!("seat loopback: Steam's UI link is on {address}");
            0
        }
        Ok(None) => 0,
        Err(error) => {
            eprintln!("seat loopback: {error}");
            1
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::super::{accounts, session};
    use super::{address, program};
    use std::net::Ipv4Addr;
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
    use std::time::{Duration, Instant};

    /// PAM and the runner's first lines: how long this start's session may take to appear.
    const SESSION_WAIT: Duration = Duration::from_secs(15);

    // `include/uapi/linux/bpf.h`.
    const BPF_PROG_LOAD: libc::c_int = 5;
    const BPF_PROG_ATTACH: libc::c_int = 8;
    const BPF_PROG_TYPE_CGROUP_SOCK_ADDR: u32 = 18;
    const BPF_CGROUP_INET4_BIND: u32 = 8;
    const BPF_CGROUP_INET4_CONNECT: u32 = 10;
    const BPF_F_ALLOW_MULTI: u32 = 2;

    /// `union bpf_attr` as `BPF_PROG_LOAD` reads it, up to the attach type.
    #[repr(C)]
    struct Load {
        prog_type: u32,
        insn_cnt: u32,
        insns: u64,
        license: u64,
        log_level: u32,
        log_size: u32,
        log_buf: u64,
        kern_version: u32,
        prog_flags: u32,
        prog_name: [u8; 16],
        prog_ifindex: u32,
        expected_attach_type: u32,
    }

    /// `union bpf_attr` as `BPF_PROG_ATTACH` reads it.
    #[repr(C)]
    struct Attach {
        target_fd: u32,
        attach_bpf_fd: u32,
        attach_type: u32,
        attach_flags: u32,
    }

    /// The address `account`'s Steam now has, `None` for a row that is not a made seat.
    pub(super) fn attach(account: &str) -> Result<Option<Ipv4Addr>, String> {
        let user = accounts::lookup(account).map_err(|e| e.to_string())?;
        let Some(user) = user.filter(accounts::is_seat) else {
            return Ok(None);
        };
        // The runner writes this inside the session this start opened.
        let session = session::runtime_dir(account).join("session");
        let deadline = Instant::now() + SESSION_WAIT;
        while !session.exists() {
            if Instant::now() >= deadline {
                return Err(format!("no session within {}s", SESSION_WAIT.as_secs()));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let slice = format!("/sys/fs/cgroup/user.slice/user-{}.slice", user.uid);
        let cgroup = std::fs::File::open(&slice).map_err(|e| format!("open {slice}: {e}"))?;
        let to = address(user.uid);
        let insns = program(to);
        // A restart inside a slice that outlived its unit attaches a second pair. It sees an
        // address already moved and passes it.
        for hook in [BPF_CGROUP_INET4_BIND, BPF_CGROUP_INET4_CONNECT] {
            let prog =
                load(&insns, hook).map_err(|e| format!("load the socket-address program: {e}"))?;
            let attr = Attach {
                target_fd: cgroup.as_raw_fd() as u32,
                attach_bpf_fd: prog.as_raw_fd() as u32,
                attach_type: hook,
                attach_flags: BPF_F_ALLOW_MULTI,
            };
            bpf(BPF_PROG_ATTACH, &attr).map_err(|e| format!("attach to {slice}: {e}"))?;
        }
        Ok(Some(to))
    }

    /// The verified program, for `hook` alone.
    #[allow(unsafe_code)]
    fn load(insns: &[u8], hook: u32) -> std::io::Result<OwnedFd> {
        // No helper is called, so the kernel asks for no particular license.
        let license = c"MIT";
        let attr = Load {
            prog_type: BPF_PROG_TYPE_CGROUP_SOCK_ADDR,
            insn_cnt: (insns.len() / 8) as u32,
            insns: insns.as_ptr() as u64,
            license: license.as_ptr() as u64,
            log_level: 0,
            log_size: 0,
            log_buf: 0,
            kern_version: 0,
            prog_flags: 0,
            prog_name: *b"pf_seat_lo\0\0\0\0\0\0",
            prog_ifindex: 0,
            expected_attach_type: hook,
        };
        let fd = bpf(BPF_PROG_LOAD, &attr)?;
        // SAFETY: the kernel made `fd` for this call alone; nothing else owns it.
        Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
    }

    /// `bpf(2)` with `attr` as its `union bpf_attr`; the kernel takes the rest as zero.
    #[allow(unsafe_code)]
    fn bpf<T>(cmd: libc::c_int, attr: &T) -> std::io::Result<libc::c_long> {
        // SAFETY: the kernel reads `size_of::<T>()` bytes of `attr`, which outlives the call,
        // and writes to no address in it: neither caller passes a log buffer. An address it
        // can't read ends the call with EFAULT.
        let ret = unsafe {
            libc::syscall(
                libc::SYS_bpf,
                cmd,
                std::ptr::from_ref(attr),
                std::mem::size_of::<T>(),
            )
        };
        if ret < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(ret)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seat_s_address_is_its_own_and_never_the_shared_one() {
        assert_eq!(address(986), Ipv4Addr::new(127, 128, 3, 218));
        assert_ne!(address(986), address(951));
        for uid in [0, 1, 1000, 65_535, u32::MAX] {
            let [net, high, ..] = address(uid).octets();
            assert_eq!((net, high & 128), (127, 128), "uid {uid}");
        }
    }

    /// The program for uid 986, as the kernel's verifier takes it on a little-endian box.
    #[test]
    #[cfg(target_endian = "little")]
    fn the_program_moves_steam_s_address_and_nothing_else() {
        #[rustfmt::skip]
        let expected = [
            0x61, 0x12, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, // r2 = ctx->user_ip4
            0x55, 0x02, 0x04, 0x00, 0x7f, 0x00, 0x00, 0x01, // not 127.0.0.1: out
            0x61, 0x12, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00, // r2 = ctx->user_port
            0x55, 0x02, 0x02, 0x00, 0xdf, 0xff, 0x00, 0x00, // not 57343: out
            0xb7, 0x02, 0x00, 0x00, 0x7f, 0x80, 0x03, 0xda, // r2 = 127.128.3.218
            0x63, 0x21, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, // ctx->user_ip4 = r2
            0xb7, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // r0 = 1
            0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // exit
        ];
        assert_eq!(program(address(986)), expected);
    }
}
