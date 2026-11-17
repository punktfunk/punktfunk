//! The bytes of the win32u GPU-preference patch: what it writes, and what it may overwrite.
//!
//! `gpu_pref::install_gpu_pref_hook` replaces the head of a win32u export with a jump. It
//! writes only over a head [`is_syscall_stub`] recognises. Pure byte tables, so the test
//! runs on every target; the patch itself is x86-64 Windows only.

// Only the x86-64 Windows patch and the tests read this module.
#![cfg_attr(
    not(all(target_os = "windows", target_arch = "x86_64")),
    allow(dead_code)
)]

/// Length of the patch, and of the head it replaces.
pub(crate) const PATCH_LEN: usize = 12;

/// `mov rax, hook; jmp rax`. An absolute jump with no trampoline: the original is never
/// called, so nothing is relocated.
pub(crate) fn jump_to(hook: usize) -> [u8; PATCH_LEN] {
    let mut patch = [0x48, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xE0];
    patch[2..10].copy_from_slice(&(hook as u64).to_le_bytes());
    patch
}

/// Whether `head` starts a win32u syscall stub nobody has patched: `mov r10, rcx`, then
/// `mov eax, <syscall number>`, then the first four bytes of
/// `test byte ptr [SharedUserData+0x308], 1`. The number differs per Windows build and is
/// not compared.
///
/// A stub is entered at its first byte only, and its one branch sits past these twelve, so a
/// jump written over them is never entered mid-instruction.
pub(crate) fn is_syscall_stub(head: &[u8; PATCH_LEN]) -> bool {
    head[..4] == [0x4C, 0x8B, 0xD1, 0xB8] && head[8..] == [0xF6, 0x04, 0x25, 0x08]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_patch_is_an_absolute_jump() {
        assert_eq!(
            jump_to(0x1122_3344_5566_7788),
            [0x48, 0xB8, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0xFF, 0xE0]
        );
    }

    #[test]
    fn only_an_untouched_syscall_stub_is_patched() {
        let stub = |number: u32| {
            let mut head = [0x4C, 0x8B, 0xD1, 0xB8, 0, 0, 0, 0, 0xF6, 0x04, 0x25, 0x08];
            head[4..8].copy_from_slice(&number.to_le_bytes());
            head
        };
        // An export head from one Windows 11 build 26200 revision. The syscall number changes
        // between updates.
        let real = [
            0x4C, 0x8B, 0xD1, 0xB8, 0xEF, 0x11, 0x00, 0x00, 0xF6, 0x04, 0x25, 0x08,
        ];
        assert!(is_syscall_stub(&real));
        assert_eq!(stub(0x11EF), real);
        assert!(is_syscall_stub(&stub(0x14A7)), "any syscall number");

        let refused: [(&str, [u8; PATCH_LEN]); 5] = [
            ("this patch, already written", jump_to(0x7FF6_0000_1000)),
            (
                "another module's relative jump",
                [
                    0xE9, 0x10, 0x20, 0x30, 0x40, 0, 0, 0, 0xF6, 0x04, 0x25, 0x08,
                ],
            ),
            (
                "a stub with no SharedUserData test",
                [
                    0x4C, 0x8B, 0xD1, 0xB8, 0x34, 0x12, 0, 0, 0x0F, 0x05, 0xC3, 0x0F,
                ],
            ),
            (
                "an ARM64 emulation thunk",
                [
                    0x48, 0x8B, 0xC4, 0x48, 0x89, 0x58, 0x20, 0x55, 0x5D, 0xE9, 0x10, 0x20,
                ],
            ),
            ("a breakpoint at the entry", {
                let mut head = stub(0x1234);
                head[0] = 0xCC;
                head
            }),
        ];
        for (what, head) in refused {
            assert!(!is_syscall_stub(&head), "{what}");
        }
    }
}
