//! The real box behind the executor (WP3.1): the extracted tree as `PayloadSource`, and the
//! seams a run is built on. `Seams::Demo` is the sandboxed fake set every preset walks;
//! `Seams::Real` probes and mutates this machine — its `root` is the extracted payload
//! (`None` only for the silent `--dry-run`, which deploys nothing).

use std::path::{Path, PathBuf};

use punktfunk_setup::platform::windows::exec::{PayloadSource, Subst};

#[derive(Clone, Debug, PartialEq)]
pub enum Seams {
    Demo {
        latency_ms: u64,
    },
    Real {
        root: Option<PathBuf>,
        version: String,
    },
}

/// `app/` → `{app}`; `staging/` stays where it is — the plan's `<staging>` points at it, and
/// the root's protected DACL is inherited, which is what the driver legs require.
pub struct DirPayload {
    pub root: PathBuf,
}

impl PayloadSource for DirPayload {
    fn deploy(&self, dest: &Path) -> Result<Vec<PathBuf>, String> {
        let app = self.root.join("app");
        if !app.is_dir() {
            return Err("this payload carries no app tree — an uninstaller cannot install".into());
        }
        let mut deferred = Vec::new();
        crate::pack::deploy_tree(&app, dest, &mut deferred)?;
        Ok(deferred)
    }
}

/// The placeholders for a real run: driver staging and the ACL'd temp under the same root.
pub fn subst(root: Option<&Path>, version: &str) -> Subst {
    let (staging, temp) = match root {
        Some(root) => {
            let tmp = root.join("tmp");
            let _ = std::fs::create_dir_all(&tmp);
            (root.join("staging"), tmp)
        }
        None => (PathBuf::from("<staging>"), std::env::temp_dir()),
    };
    let env = punktfunk_setup::seam::Env::from_env();
    let var = |k: &str| env.get(k).unwrap_or_default().to_string();
    let (desktop, programs) = punktfunk_setup::platform::windows::sys::shell_folders();
    Subst {
        version: version.to_string(),
        staging: staging.display().to_string(),
        temp: temp.display().to_string(),
        local_app_data: var("LOCALAPPDATA"),
        start_menu: programs.unwrap_or_else(|| {
            format!(
                "{}\\Microsoft\\Windows\\Start Menu\\Programs",
                var("APPDATA")
            )
        }),
        desktop: desktop.unwrap_or_else(|| format!("{}\\Desktop", var("USERPROFILE"))),
    }
}
