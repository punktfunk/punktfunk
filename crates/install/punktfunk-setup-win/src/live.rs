//! The seams one run owns while its executor runs: the demo's fakes, or the real box's system
//! runner, NLA probe and extracted payload. The wizard thread and the silent entry both build
//! one from a [`real::Seams`] choice.

use punktfunk_setup::platform::windows::demo::{sandbox_app_dir, WinDemoRunner, WinPreset};
use punktfunk_setup::platform::windows::exec::{FakePayload, PayloadSource, Subst, WinExecutor};
use punktfunk_setup::platform::windows::{base_paths, FakeNet, NetProbe, SystemNet};
use punktfunk_setup::seam::{CommandRunner, Env, SystemRunner};
use punktfunk_setup::ui::Reporter;

use crate::real::{self, DirPayload};

/// The demo's filesystem: everything the executor may write goes under the same per-process
/// sandbox root as `demo::sandbox_paths` — including the "install dir" the uninstall preset
/// removes, staged here with a marker so the removal has something honest to do.
pub(crate) fn stage_demo_tree() -> String {
    let app = sandbox_app_dir();
    let _ = std::fs::create_dir_all(&app);
    let _ = std::fs::write(
        std::path::Path::new(&app).join("installed-by-demo.txt"),
        "demo install marker\n",
    );
    let tmp = std::env::temp_dir()
        .join(format!("punktfunk-setup-demo-{}", std::process::id()))
        .join("tmp");
    let _ = std::fs::create_dir_all(&tmp);
    tmp.display().to_string()
}

/// The demo's seams — the same objects `silent` builds, owned for the thread's lifetime.
pub struct DemoSeams {
    pub run: WinDemoRunner,
    pub net: FakeNet,
    pub payload: FakePayload,
    pub paths: punktfunk_setup::seam::BasePaths,
    pub subst: Subst,
}

impl DemoSeams {
    pub fn new(preset: &WinPreset, latency_ms: u64) -> DemoSeams {
        let tmp = stage_demo_tree();
        DemoSeams {
            run: WinDemoRunner::new(latency_ms, None),
            net: FakeNet {
                networks: preset.facts.networks.clone(),
                ..FakeNet::default()
            },
            payload: FakePayload::default(),
            paths: punktfunk_setup::demo::sandbox_paths(),
            subst: Subst {
                version: concat!(env!("CARGO_PKG_VERSION"), "-demo").to_string(),
                staging: format!("{tmp}\\staging"),
                local_app_data: format!("{tmp}\\localappdata"),
                start_menu: format!("{tmp}\\startmenu"),
                desktop: format!("{tmp}\\desktop"),
                common_start_menu: format!("{tmp}\\commonstartmenu"),
                temp: tmp,
            },
        }
    }
}

/// The real box's seams: system runner + NLA probe, `%ProgramData%` paths, the
/// extracted tree as payload (a `FakePayload` when there is none — dry runs only).
pub struct RealSeams {
    pub run: SystemRunner,
    pub net: SystemNet,
    pub payload: Box<dyn PayloadSource>,
    pub paths: punktfunk_setup::seam::BasePaths,
    pub subst: Subst,
}

impl RealSeams {
    pub fn new(root: Option<&std::path::Path>, version: &str) -> RealSeams {
        RealSeams {
            run: SystemRunner::new(),
            net: SystemNet,
            payload: match root {
                Some(root) => Box::new(DirPayload {
                    root: root.to_path_buf(),
                }),
                None => Box::new(FakePayload::default()),
            },
            paths: base_paths(&Env::from_env()),
            subst: real::subst(root, version),
        }
    }
}

/// One run's seams, owned for as long as its executor runs.
pub enum LiveSeams {
    Demo(DemoSeams),
    Real(RealSeams),
}

impl LiveSeams {
    pub fn executor<'a>(
        &'a self,
        ui: &'a dyn Reporter,
        dry: bool,
        silent: bool,
        web_password: Option<String>,
    ) -> WinExecutor<'a> {
        let (run, net, payload, paths, subst): (
            &dyn CommandRunner,
            &dyn NetProbe,
            &dyn PayloadSource,
            &punktfunk_setup::seam::BasePaths,
            &Subst,
        ) = match self {
            LiveSeams::Demo(d) => (&d.run, &d.net, &d.payload, &d.paths, &d.subst),
            LiveSeams::Real(r) => (&r.run, &r.net, r.payload.as_ref(), &r.paths, &r.subst),
        };
        WinExecutor {
            run,
            net,
            payload,
            paths,
            ui,
            dry,
            silent,
            web_password,
            subst: subst.clone(),
        }
    }
}
