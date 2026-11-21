//! One virtual pad through its real manager, for a controller reader to prove: the south button
//! toggles every 500 ms, the left stick sweeps, the pad turns at 100 °/s, and the feedback the
//! reader sends prints here. Runs from an app bundle signed with the HID virtual device
//! entitlement; unsigned, the create is refused.
//!
//! Usage: `mac_pads <dualsense|edge|ds4|xbox|xboxone|elite|switchpro> [seconds, at most 20]`

#[cfg(target_os = "macos")]
fn main() {
    mac::main();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("mac_pads runs on macOS");
}

#[cfg(target_os = "macos")]
mod mac {
    use pf_inject::dualsense_macos::{DsMacProto, DualSenseMacManager};
    use pf_inject::dualshock4_macos::DualShock4MacManager;
    use pf_inject::switch_pro_macos::SwitchProMacManager;
    use pf_inject::uhid_manager::{PadProto, UhidManager};
    use pf_inject::xbox_macos::{XboxMacManager, XboxMacProto};
    use punktfunk_core::input::gamepad::{
        BTN_A, MOTION_ACCEL_LSB_PER_G, MOTION_GYRO_LSB_PER_DEG_S,
    };
    use punktfunk_core::input::{GamepadEvent, GamepadFrame};
    use punktfunk_core::quic::RichInput;
    use std::time::{Duration, Instant};

    pub fn main() {
        let mut args = std::env::args().skip(1);
        let kind = args.next().unwrap_or_default();
        let secs = args
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(15u64)
            .min(20);
        println!("probe: {:?}", pf_inject::hid_device_probe());
        match kind.as_str() {
            "dualsense" => run(DualSenseMacManager::new(), secs),
            "edge" => run(DualSenseMacManager::with_backend(DsMacProto::edge()), secs),
            "ds4" => run(DualShock4MacManager::new(), secs),
            // The default pad, as the host builds it.
            "xbox" => run(pf_inject::gamepad::GamepadManager::new(), secs),
            "xboxone" => run(XboxMacManager::with_backend(XboxMacProto::one_s()), secs),
            "elite" => run(XboxMacManager::with_backend(XboxMacProto::elite()), secs),
            "switchpro" => run(SwitchProMacManager::new(), secs),
            _ => {
                eprintln!(
                    "usage: mac_pads <dualsense|edge|ds4|xbox|xboxone|elite|switchpro> [secs]"
                );
                std::process::exit(2);
            }
        }
    }

    fn run<B: PadProto>(mut pads: UhidManager<B>, secs: u64) {
        pads.handle(&GamepadEvent::Arrival {
            index: 0,
            kind: 0,
            capabilities: 0,
            audio_caps: 0,
        });
        if pads.live_pads() != 1 {
            eprintln!("pad not created");
            std::process::exit(1);
        }
        println!("pad up for {secs} s");
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(secs) {
            let ms = start.elapsed().as_millis() as i32;
            pads.handle(&GamepadEvent::State(GamepadFrame {
                index: 0,
                active_mask: 1,
                buttons: if (ms / 500) % 2 == 1 { BTN_A } else { 0 },
                ls_x: (((ms / 8) % 256 - 128) * 256) as i16,
                ..GamepadFrame::default()
            }));
            pads.apply_rich(RichInput::Motion {
                pad: 0,
                gyro: [(100 * MOTION_GYRO_LSB_PER_DEG_S) as i16, 0, 0],
                accel: [0, MOTION_ACCEL_LSB_PER_G as i16, 0],
            });
            pads.pump(
                |pad, low, high, lt, rt| {
                    println!("rumble pad={pad} low={low} high={high} lt={lt} rt={rt}")
                },
                |h| println!("hidout {h:?}"),
            );
            pads.heartbeat(Duration::from_millis(8));
            std::thread::sleep(Duration::from_millis(4));
        }
    }
}
