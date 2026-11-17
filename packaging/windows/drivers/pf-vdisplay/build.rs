//! WDK link flags for the cdylib (wdk-build) + `IddCxStub` (the driver calls IddCx DDIs via wdk-iddcx,
//! and exports `IddMinimumVersionRequired`). `/INTEGRITYCHECK` (set by wdk-build) is cleared by the CI
//! packaging step.
fn main() -> Result<(), wdk_build::ConfigError> {
    wdk_build::configure_wdk_binary_build()?;
    link_iddcx_stub();
    Ok(())
}

/// Link `IddCxStub.lib`. It ships under `Lib\<sdkver>\um\<arch>\iddcx\<iddcxver>\`, and the iddcx
/// versions are NOT interchangeable: the stub's `IddFunctions` dispatch table must match the
/// running framework (a `1.0` stub fails even `IddCxDeviceInitConfig`). So pick the HIGHEST
/// `iddcx\<X.Y>` dir that has the lib (version-aware, since "1.10" < "1.2" lexically). `<arch>`
/// follows the cargo target: `x64` or `ARM64`.
fn link_iddcx_stub() {
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").expect("CARGO_CFG_TARGET_ARCH");
    let cpu = wdk_build::CpuArchitecture::try_from_cargo_str(&target_arch)
        .unwrap_or_else(|| panic!("no WDK lib dir for target arch {target_arch}"));
    let arch = cpu.as_windows_str();
    const ROOTS: [&str; 2] = [
        r"C:\Program Files (x86)\Windows Kits\10\Lib",
        r"C:\Program Files\Windows Kits\10\Lib",
    ];
    let mut best: Option<((u32, u32), std::path::PathBuf)> = None;
    for root in ROOTS {
        let Ok(versions) = std::fs::read_dir(root) else {
            continue;
        };
        for ver in versions.flatten() {
            let iddcx = ver.path().join("um").join(arch).join("iddcx");
            let Ok(subdirs) = std::fs::read_dir(&iddcx) else {
                continue;
            };
            for sub in subdirs.flatten() {
                if !sub.path().join("IddCxStub.lib").is_file() {
                    continue;
                }
                let name = sub.file_name().to_string_lossy().into_owned();
                let mut parts = name.split('.');
                let v = (
                    parts.next().and_then(|x| x.parse().ok()).unwrap_or(0u32),
                    parts.next().and_then(|x| x.parse().ok()).unwrap_or(0u32),
                );
                if best.as_ref().is_none_or(|(bv, _)| v > *bv) {
                    best = Some((v, sub.path()));
                }
            }
        }
    }
    let Some((_, dir)) = best else {
        panic!(
            "IddCxStub.lib not found under any Windows Kits Lib\\<ver>\\um\\{arch}\\iddcx\\<iddcxver>\\"
        );
    };
    println!("cargo:rustc-link-search={}", dir.display());
    println!("cargo:rustc-link-lib=static=IddCxStub");
}
