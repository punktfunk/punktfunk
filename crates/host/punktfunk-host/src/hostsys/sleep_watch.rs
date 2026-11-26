//! `host.sleeping`: hooks hear that the machine is about to sleep, and the sleep waits for the
//! ones that hold it.
//!
//! The OS announces a sleep and waits a bounded time for an answer. [`sleeping`] emits the
//! event and runs every hook with `hold` set on it; the watcher answers once they are done.
//! Every sleep passes here: a power menu, a lid, an idle timer, the host's own `power.sleep`.
//!
//! | OS | Announced by | The OS waits |
//! |---|---|---|
//! | Linux | logind `PrepareForSleep`, under a `delay` inhibitor | `InhibitDelayMaxSec`, 5 s unless raised |
//! | Windows | `PowerRegisterSuspendResumeNotification` | about 2 s |
//! | macOS | `IORegisterForSystemPower` | 30 s |
//!
//! A hook that outlasts the wait is frozen with the machine and finishes after the wake.
//! Nothing here can refuse a sleep: with no hook holding, the answer is immediate.

use std::time::Instant;

/// Emit `host.sleeping` and block until every hook holding it has finished or timed out.
fn sleeping() {
    let ev = crate::events::bus().emit(crate::events::EventKind::HostSleeping);
    let hooks = crate::hooks::holding(&ev);
    if hooks.is_empty() {
        return;
    }
    let started = Instant::now();
    std::thread::scope(|s| {
        for h in &hooks {
            s.spawn(|| crate::hooks::hold(h, &ev));
        }
    });
    tracing::info!(
        hooks = hooks.len(),
        ms = started.elapsed().as_millis() as u64,
        "sleep hold released"
    );
}

/// Watch for sleeps for the life of the process. Call once, on a host that runs hooks.
pub fn spawn() {
    #[cfg(any(target_os = "linux", windows, target_os = "macos"))]
    if let Err(e) = std::thread::Builder::new()
        .name("pf-sleep-watch".into())
        .spawn(watch)
    {
        tracing::warn!(error = %e, "sleep watch thread did not start");
    }
}

/// Holds a logind `delay` inhibitor while awake, and gives it up once the hooks are done.
#[cfg(target_os = "linux")]
fn watch() {
    use futures_util::StreamExt as _;
    use zbus::zvariant::OwnedFd;

    async fn delay(login: &zbus::Proxy<'_>) -> Option<OwnedFd> {
        let what = "sleep";
        let args = (what, "Punktfunk", "hooks run before a sleep", "delay");
        let fd = match login.call_method("Inhibit", &args).await {
            Ok(reply) => reply.body().deserialize::<OwnedFd>(),
            Err(e) => Err(e),
        };
        fd.inspect_err(|e| {
            tracing::warn!(error = %e, "logind delay inhibitor refused: a sleep won't wait for hooks");
        })
        .ok()
    }

    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    let watched: zbus::Result<()> = rt.block_on(async {
        let conn = zbus::Connection::system().await?;
        let login = zbus::Proxy::new(
            &conn,
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
        )
        .await?;
        let mut signals = login.receive_signal("PrepareForSleep").await?;
        let mut held = delay(&login).await;
        while let Some(msg) = signals.next().await {
            let Ok((going_down,)) = msg.body().deserialize::<(bool,)>() else {
                continue;
            };
            if going_down {
                // Off this thread, so the connection keeps reading while hooks run.
                let _ = tokio::task::spawn_blocking(sleeping).await;
                // Closing the pipe is the answer: logind goes on once every holder has.
                drop(held.take());
            } else {
                held = delay(&login).await;
            }
        }
        Ok(())
    });
    match watched {
        Ok(()) => tracing::warn!("logind stopped announcing sleeps: host.sleeping won't fire"),
        Err(e) => {
            tracing::info!(error = %e, "no logind to announce sleeps: host.sleeping won't fire")
        }
    }
}

/// Windows calls back on one of its own threads and suspends when the callback returns.
#[cfg(windows)]
fn watch() {
    use std::ffi::c_void;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Power::{
        PowerRegisterSuspendResumeNotification, DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS,
    };
    use windows::Win32::UI::WindowsAndMessaging::{DEVICE_NOTIFY_CALLBACK, PBT_APMSUSPEND};

    unsafe extern "system" fn on_power(
        _ctx: *const c_void,
        kind: u32,
        _setting: *const c_void,
    ) -> u32 {
        if kind == PBT_APMSUSPEND {
            // A panic must not cross into Windows.
            let _ = std::panic::catch_unwind(sleeping);
        }
        0
    }

    // Leaked: the registration lives as long as the process.
    let params = Box::leak(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
        Callback: Some(on_power),
        Context: std::ptr::null_mut(),
    }));
    let mut handle = std::ptr::null_mut();
    // SAFETY: `params` is leaked, so it outlives the registration, and `handle` is a live
    // out-param. DEVICE_NOTIFY_CALLBACK makes `recipient` a pointer to that struct.
    let rc = unsafe {
        PowerRegisterSuspendResumeNotification(
            DEVICE_NOTIFY_CALLBACK,
            HANDLE(std::ptr::from_mut(params).cast()),
            &mut handle,
        )
    };
    if rc.is_err() {
        tracing::warn!(
            rc = rc.0,
            "suspend notification refused: host.sleeping won't fire"
        );
    }
}

/// The root power domain's connection, which every answer names. Set once, before the loop runs.
#[cfg(target_os = "macos")]
static ROOT_PORT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Runs a run loop of its own for the power notifications, so hooks never block the app's.
#[cfg(target_os = "macos")]
fn watch() {
    use objc2_core_foundation::{kCFRunLoopDefaultMode, CFRunLoop};
    use objc2_io_kit::{
        io_object_t, io_service_t, kIOMessageCanSystemSleep, kIOMessageSystemWillSleep,
        IOAllowPowerChange, IONotificationPort, IONotificationPortRef, IORegisterForSystemPower,
    };
    use std::ffi::c_void;
    use std::sync::atomic::Ordering;

    unsafe extern "C-unwind" fn on_power(
        _: *mut c_void,
        _: io_service_t,
        kind: u32,
        id: *mut c_void,
    ) {
        // An idle sleep asks first and a forced one only tells: both wait for this answer.
        if kind == kIOMessageSystemWillSleep {
            let _ = std::panic::catch_unwind(sleeping);
        }
        if kind == kIOMessageSystemWillSleep || kind == kIOMessageCanSystemSleep {
            IOAllowPowerChange(ROOT_PORT.load(Ordering::Relaxed), id as isize);
        }
    }

    let mut port: IONotificationPortRef = std::ptr::null_mut();
    let mut notifier: io_object_t = 0;
    // SAFETY: both out-params are live locals, the callback matches IOServiceInterestCallback
    // and reads no refcon, so a null one is never dereferenced.
    let root = unsafe {
        IORegisterForSystemPower(
            std::ptr::null_mut(),
            &mut port,
            Some(on_power),
            &mut notifier,
        )
    };
    // SAFETY: `port` is the live notification port the call above returned on success.
    let source = (root != 0)
        .then(|| unsafe { IONotificationPort::run_loop_source(port) })
        .flatten();
    let (Some(source), Some(run_loop)) = (source, CFRunLoop::current()) else {
        tracing::warn!("power notifications refused: host.sleeping won't fire");
        return;
    };
    ROOT_PORT.store(root, Ordering::Relaxed);
    // SAFETY: the mode is a CoreFoundation static.
    run_loop.add_source(Some(&source), unsafe { kCFRunLoopDefaultMode });
    CFRunLoop::run();
}
