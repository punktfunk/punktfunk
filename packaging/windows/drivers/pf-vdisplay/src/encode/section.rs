//! The driver's side of the AU section: the handle values a `SET_ENCODE` carries, mapped
//! through the audited view and handed to the shared writer ([`AuSection`]).

use core::sync::atomic::{AtomicU32, AtomicU64};

use pf_driver_proto::encode::au;
use pf_encode_session::section::SectionView;
use pf_umdf_util::section::{self, MappedView};

use super::convert::Fail;

pub use pf_encode_session::section::{AuSection, Ctl, EncodeSession};

/// [`MappedView`] as the shared writer's view.
struct View(MappedView);

impl SectionView for View {
    fn atomic_u32(&self, off: usize) -> &AtomicU32 {
        self.0.atomic_u32(off)
    }

    fn atomic_u64(&self, off: usize) -> &AtomicU64 {
        self.0.atomic_u64(off)
    }

    fn read_bytes(&self, off: usize, dst: &mut [u8]) {
        self.0.read_bytes(off, dst);
    }

    fn copy_from_slice(&self, off: usize, src: &[u8]) {
        self.0.copy_from_slice(off, src);
    }
}

/// Map `section` and adopt both handles. `Err` means NOTHING was adopted: the values are
/// left for the host to reap alongside the IOCTL's failure status.
pub fn map(section: u64, event: u64, section_bytes: u32) -> Result<AuSection, Fail> {
    // A short section is refused before anything is mapped, not faulted on.
    if (section_bytes as usize) < au::HEAP_OFFSET {
        dbglog!("[pf-vd] encode: AU section of {section_bytes} B is shorter than its layout");
        return Err((-9, "section"));
    }
    let Some(view) = MappedView::from_handle_value(section, section_bytes as usize) else {
        dbglog!(
            "[pf-vd] encode: MapViewOfFile({section_bytes} B) failed: {:?}",
            windows::core::Error::from_win32()
        );
        return Err((-9, "map"));
    };
    let event = event as usize as *mut core::ffi::c_void;
    // SAFETY: `event` is the ready event the host duplicated into this process for this
    // section; the writer is its sole closer from here. A refusal adopts nothing, and the
    // view unmaps as it drops.
    let adopted = unsafe { AuSection::adopt(Box::new(View(view)), section_bytes, event) }?;
    // The view keeps the section alive, so the duplicated handle can close now.
    section::close_handle_value(section);
    Ok(adopted)
}
