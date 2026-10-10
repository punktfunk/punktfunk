//! `PUNKTFUNK_PERF` stage timings for the client pump and host send thread.
//! Accumulated per report window; drain with
//! [`Session::take_pump_perf`](super::Session::take_pump_perf) /
//! [`Session::take_seal_perf`](super::Session::take_seal_perf).

use crate::fec::ErasureCoder;

/// Client receive-path stage timings since the last
/// [`Session::take_pump_perf`](super::Session::take_pump_perf).
#[derive(Debug, Default, Clone, Copy)]
pub struct PumpPerf {
    /// `recv_batch` (recvmmsg / recvmsg_x): syscall + kernel copy.
    pub recv_ns: u64,
    /// `open_in_place` (AES-128-GCM + replay-window upkeep).
    pub decrypt_ns: u64,
    /// `Reassembler::push` (parse, shard copy, FEC reconstruct, AU assembly).
    pub reasm_ns: u64,
    /// `recv_batch` calls and datagrams in the window.
    pub batches: u64,
    pub packets: u64,
}

/// Host send-path stage timings since the last
/// [`Session::take_seal_perf`](super::Session::take_seal_perf). Paced video folds
/// its chunk sends into `sock_ns` via
/// [`Session::note_sock_ns`](super::Session::note_sock_ns).
#[derive(Debug, Default, Clone, Copy)]
pub struct SealPerf {
    /// [`ErasureCoder::encode_into`] (parity).
    pub fec_ns: u64,
    /// `seal_in_place` (AES-128-GCM) across all wire packets.
    pub seal_ns: u64,
    /// `send_sealed` socket syscalls, plus paced chunks via `note_sock_ns`.
    pub sock_ns: u64,
    pub frames: u64,
    pub packets: u64,
}

/// Times `encode_into` into [`SealPerf`] when `PUNKTFUNK_PERF` is armed. `ns` is
/// atomic only to satisfy [`ErasureCoder`]'s `Sync` bound; it lives on one thread.
pub(super) struct TimedCoder<'a> {
    pub(super) inner: &'a dyn ErasureCoder,
    pub(super) ns: &'a std::sync::atomic::AtomicU64,
}

impl ErasureCoder for TimedCoder<'_> {
    fn scheme(&self) -> crate::config::FecScheme {
        self.inner.scheme()
    }
    fn encode(
        &self,
        data: &[&[u8]],
        recovery_count: usize,
    ) -> std::result::Result<Vec<Vec<u8>>, crate::fec::FecError> {
        self.inner.encode(data, recovery_count)
    }
    fn encode_into(
        &self,
        data: &[&[u8]],
        recovery_count: usize,
        out: &mut Vec<Vec<u8>>,
    ) -> std::result::Result<(), crate::fec::FecError> {
        let t0 = std::time::Instant::now();
        let r = self.inner.encode_into(data, recovery_count, out);
        self.ns.fetch_add(
            t0.elapsed().as_nanos() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        r
    }
    fn reconstruct(
        &self,
        data_count: usize,
        recovery_count: usize,
        received: &mut [Option<Vec<u8>>],
    ) -> std::result::Result<Vec<Vec<u8>>, crate::fec::FecError> {
        self.inner.reconstruct(data_count, recovery_count, received)
    }
    fn reconstruct_into(
        &self,
        recovery_count: usize,
        data: &mut [&mut [u8]],
        have: &[bool],
        recovery: &[(usize, &[u8])],
    ) -> std::result::Result<(), crate::fec::FecError> {
        self.inner
            .reconstruct_into(recovery_count, data, have, recovery)
    }
}
