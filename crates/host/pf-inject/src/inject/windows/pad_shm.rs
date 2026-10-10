//! One virtual HID pad on the `pf_gamepad` UMDF driver: the sealed `PadShm` channel, its
//! devnode and the attach watcher. Every identity (DualSense, Edge, DualShock 4, Deck, Triton,
//! Switch Pro, Xbox) opens through [`ShmPad::open`] and supplies only its device type, neutral
//! report and PnP identity. The section layout and ring reader are [`crate::pad_shm_ring`];
//! the channel is `design/gamepad-channel-sealing.md`.

use super::gamepad_raii::{
    create_swdevice, DriverAttach, PadChannel, ProofTransport, SwDevice, SwDeviceProfile,
};
use crate::pad_shm_ring::{driver_marks, publish_input, stamp, OutputDrain, SHM_SIZE};
use anyhow::Result;
use std::time::Duration;

/// The one driver package every pad identity installs from.
const GAMEPAD_INF: &str = "pf_gamepad.inf";
/// Where debug builds of that driver log.
const GAMEPAD_DRIVER_LOG: &str =
    "C:\\Windows\\ServiceProfiles\\LocalService\\AppData\\Local\\Temp\\pf_gamepad-driver.log";

/// A pad's devnode plus its sealed section. Drop removes the devnode, then closes both sections.
pub(super) struct ShmPad {
    _sw: SwDevice,
    channel: PadChannel,
    attach: DriverAttach,
    /// v2.3 input-seqlock generation — see [`publish_input`].
    input_gen: u32,
    drain: OutputDrain,
}

impl ShmPad {
    /// Create the sealed channel, [`stamp`] it, spawn the devnode and deliver the section.
    pub(super) fn open(
        index: u8,
        devtype: u8,
        neutral: &[u8],
        profile: &SwDeviceProfile,
    ) -> Result<ShmPad> {
        let boot_name = pf_driver_proto::gamepad::pad_boot_name(index);
        let mut channel = PadChannel::create(boot_name.clone(), SHM_SIZE)?;
        // Ring version 2: this host drains the v2.2 long ring.
        stamp(channel.data(), devtype, index, 2, neutral);
        // `?`: PadSlots retries a failed create; a swallowed one latched a pad with no devnode.
        let (sw, instance_id) = create_swdevice(profile)?;
        // Duplicate into the process serving this devnode, not the pid the LocalService-writable
        // mailbox names.
        channel.bind_devnode(
            index.into(),
            instance_id.clone(),
            ProofTransport::HidFeatureReport,
        );
        // The driver must hold the section (and read `device_type`) before hidclass asks for
        // descriptors, or the pad enumerates as a DualSense. 1500 ms bounds that wait.
        channel.deliver_eager(Duration::from_millis(1500));
        Ok(ShmPad {
            _sw: sw,
            channel,
            attach: DriverAttach::new(
                profile.hwid,
                GAMEPAD_INF,
                GAMEPAD_DRIVER_LOG,
                boot_name,
                instance_id,
            ),
            input_gen: 0,
            drain: OutputDrain::new(),
        })
    }

    /// Publish one input report, cut to the input slot. The driver's timer copies the whole
    /// slot; there is no change-detect on this plane.
    pub(super) fn publish(&mut self, report: &[u8]) {
        publish_input(self.channel.data(), &mut self.input_gen, report);
    }

    /// One service tick: pump channel delivery, feed the attach watcher, then hand every output
    /// report to `per_report` oldest first ([`OutputDrain::drain_tagged`]). Returns `true` on a
    /// ring overflow, which the caller forwards as `PadFeedback::resync`.
    pub(super) fn poll(&mut self, per_report: impl FnMut(&[u8], bool)) -> bool {
        self.channel.pump();
        let shm = self.channel.data();
        let (proto, rev) = driver_marks(shm);
        self.attach.observe_pad(proto, rev);
        self.drain.drain_tagged(shm, per_report)
    }

    /// The bootstrap mailbox this pad's driver attaches through, for log lines.
    pub(super) fn mailbox(&self) -> &str {
        self.channel.boot_name()
    }
}
