//! Per-buffer acquire and release timelines for the native lane's explicit sync.
//!
//! With `wp_linux_drm_syncobj_v1` on a surface the compositor waits a buffer's acquire point
//! before reading it and signals its release point when done; `wl_buffer.release` is no longer
//! reliable. Each buffer gets its own pair: the protocol warns that a shared release timeline
//! can be signalled out of order. The timelines are exportable Vulkan timeline semaphores; on
//! Mesa their opaque fd is the DRM syncobj the compositor imports.

use anyhow::{Context as _, Result};
use ash::vk;
use std::os::fd::{AsFd as _, BorrowedFd, FromRawFd as _, OwnedFd};

/// Makes [`Timelines`]; `None` from [`TimelineMaker::new`] where timeline semaphores cannot be
/// exported as opaque fds.
pub(crate) struct TimelineMaker {
    ext: ash::khr::external_semaphore_fd::Device,
}

/// One buffer's acquire and release timelines.
pub(crate) struct Timelines {
    pub(crate) acquire: vk::Semaphore,
    pub(crate) release: vk::Semaphore,
    acquire_fd: OwnedFd,
    release_fd: OwnedFd,
    /// Last point handed out; the next commit uses the one after.
    point: u64,
    /// Point of the commit the compositor may still hold the buffer for.
    committed: Option<u64>,
}

impl TimelineMaker {
    /// # Safety
    ///
    /// `instance`, `pdev` and `device` are live and paired; the device enabled
    /// VK_KHR_external_semaphore_fd and the timelineSemaphore feature.
    pub(crate) unsafe fn new(
        instance: &ash::Instance,
        pdev: vk::PhysicalDevice,
        device: &ash::Device,
    ) -> Option<Self> {
        let mut ty = vk::SemaphoreTypeCreateInfo::default()
            .semaphore_type(vk::SemaphoreType::TIMELINE)
            .initial_value(0);
        let info = vk::PhysicalDeviceExternalSemaphoreInfo::default()
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD)
            .push_next(&mut ty);
        let mut props = vk::ExternalSemaphoreProperties::default();
        // SAFETY: read-only query on live handles; the chain roots locals outliving the call.
        unsafe {
            instance.get_physical_device_external_semaphore_properties(pdev, &info, &mut props)
        };
        props
            .external_semaphore_features
            .contains(vk::ExternalSemaphoreFeatureFlags::EXPORTABLE)
            .then(|| Self {
                ext: ash::khr::external_semaphore_fd::Device::new(instance, device),
            })
    }

    /// # Safety
    ///
    /// `device` is the one this maker was made for.
    pub(crate) unsafe fn make(&self, device: &ash::Device) -> Result<Timelines> {
        // SAFETY: fn contract.
        let (acquire, acquire_fd) = unsafe { self.one(device)? };
        // SAFETY: fn contract.
        let (release, release_fd) = match unsafe { self.one(device) } {
            Ok(r) => r,
            Err(e) => {
                // SAFETY: created above, never submitted.
                unsafe { device.destroy_semaphore(acquire, None) };
                return Err(e);
            }
        };
        Ok(Timelines {
            acquire,
            release,
            acquire_fd,
            release_fd,
            point: 0,
            committed: None,
        })
    }

    /// # Safety
    ///
    /// As [`Self::make`].
    unsafe fn one(&self, device: &ash::Device) -> Result<(vk::Semaphore, OwnedFd)> {
        let mut ty = vk::SemaphoreTypeCreateInfo::default()
            .semaphore_type(vk::SemaphoreType::TIMELINE)
            .initial_value(0);
        let mut export = vk::ExportSemaphoreCreateInfo::default()
            .handle_types(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD);
        let ci = vk::SemaphoreCreateInfo::default()
            .push_next(&mut ty)
            .push_next(&mut export);
        // SAFETY: fn contract; the chain roots locals outliving the call.
        let sem = unsafe { device.create_semaphore(&ci, None) }.context("timeline semaphore")?;
        let gi = vk::SemaphoreGetFdInfoKHR::default()
            .semaphore(sem)
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD);
        // SAFETY: `sem` was created exportable as an opaque fd just above.
        match unsafe { self.ext.get_semaphore_fd(&gi) } {
            // SAFETY: the driver hands us a fresh fd we now own.
            Ok(fd) => Ok((sem, unsafe { OwnedFd::from_raw_fd(fd) })),
            Err(e) => {
                // SAFETY: created above, never submitted.
                unsafe { device.destroy_semaphore(sem, None) };
                Err(e).context("vkGetSemaphoreFdKHR (opaque)")
            }
        }
    }
}

impl Timelines {
    /// The fds the compositor imports: (acquire, release).
    pub(crate) fn fds(&self) -> (BorrowedFd<'_>, BorrowedFd<'_>) {
        (self.acquire_fd.as_fd(), self.release_fd.as_fd())
    }

    /// The point the next commit of this buffer uses, recorded as committed.
    pub(crate) fn next_point(&mut self) -> u64 {
        self.point += 1;
        self.committed = Some(self.point);
        self.point
    }

    /// A commit that never happened: the buffer is not held for it.
    pub(crate) fn uncommit(&mut self) {
        self.committed = None;
    }

    /// A commit's release point is still owed.
    pub(crate) fn is_committed(&self) -> bool {
        self.committed.is_some()
    }

    /// Whether the compositor still holds the buffer for a commit. Clears once released.
    ///
    /// # Safety
    ///
    /// `device` owns the semaphores.
    pub(crate) unsafe fn poll_held(&mut self, device: &ash::Device) -> bool {
        let Some(point) = self.committed else {
            return false;
        };
        // SAFETY: fn contract.
        let value = unsafe { device.get_semaphore_counter_value(self.release) }.unwrap_or(0);
        if value >= point {
            self.committed = None;
            return false;
        }
        true
    }

    /// Signal the acquire point from the host: the content is already in memory.
    ///
    /// # Safety
    ///
    /// `device` owns the semaphores; `point` is above every earlier acquire point.
    pub(crate) unsafe fn signal_acquire(&self, device: &ash::Device, point: u64) -> Result<()> {
        let info = vk::SemaphoreSignalInfo::default()
            .semaphore(self.acquire)
            .value(point);
        // SAFETY: fn contract.
        unsafe { device.signal_semaphore(&info) }.context("vkSignalSemaphore (acquire)")
    }

    /// # Safety
    ///
    /// No pending submit or compositor wait still names the semaphores' Vulkan side; the
    /// compositor keeps its own syncobj references.
    pub(crate) unsafe fn destroy(self, device: &ash::Device) {
        // SAFETY: fn contract.
        unsafe {
            device.destroy_semaphore(self.acquire, None);
            device.destroy_semaphore(self.release, None);
        }
    }
}
