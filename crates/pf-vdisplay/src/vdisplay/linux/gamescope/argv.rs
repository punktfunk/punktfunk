//! A running gamescope's command line: which processes are gamescope, the flag values they carry,
//! and whether a session came up at the size and with the flags the host asked for. gamescope
//! parses with `getopt_long`, so a long flag's value is the next token or follows `=`; every
//! reader here takes both.

use super::*;

/// Compositor argv from `/proc/<pid>/cmdline`. Basename `ends_with("gamescope")` — `/proc/…/exe`
/// is often unreadable, and `==` would miss `punktfunk-gamescope` while still excluding helpers.
pub(super) fn gamescope_argvs() -> Vec<Vec<String>> {
    argvs_where(|_| true)
}

/// [`gamescope_argvs`] of the processes `scope` admits.
pub(super) fn scoped_argvs(scope: Scope<'_>) -> Vec<Vec<String>> {
    argvs_where(|pid| scope.rank(Some(pid)).is_some())
}

fn argvs_where(keep: impl Fn(u32) -> bool) -> Vec<Vec<String>> {
    crate::proc::pids()
        .filter_map(|(pid, path)| {
            let raw = std::fs::read(path.join("cmdline")).ok()?;
            let args: Vec<String> = raw
                .split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect();
            let a0 = args.first()?;
            let gamescope = a0.rsplit('/').next().unwrap_or(a0).ends_with("gamescope");
            (gamescope && keep(pid)).then_some(args)
        })
        .collect()
}

/// Value of the first matching flag, in `--flag value` and `--flag=value` form.
pub(super) fn flag_value<'a>(argv: &'a [String], names: &[&str]) -> Option<&'a str> {
    argv.iter().enumerate().find_map(|(i, a)| {
        if let Some((k, v)) = a.split_once('=') {
            if names.contains(&k) {
                return Some(v);
            }
        }
        if names.contains(&a.as_str()) {
            return argv.get(i + 1).map(|s| s.as_str());
        }
        None
    })
}

pub(super) fn argv_u32(argv: &[String], names: &[&str]) -> Option<u32> {
    flag_value(argv, names)?.parse().ok()
}

/// `-W`/`-H` of one argv. `None` if either is missing — also the compositor vs helper filter.
pub(super) fn gamescope_output_size(argv: &[String]) -> Option<(u32, u32)> {
    match (
        argv_u32(argv, &["-W", "--output-width"]),
        argv_u32(argv, &["-H", "--output-height"]),
    ) {
        (Some(w), Some(h)) => Some((w, h)),
        _ => None,
    }
}

/// Three states: Game Mode routinely runs a session compositor plus a nested per-title gamescope.
/// Collapsing unknown with a different size would restart the box unit and kill the running game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BoxOutputSize {
    /// Unanimous `-W`/`-H`. The only state a caller may act on.
    Known((u32, u32)),
    /// No output size reported. Re-mode may proceed: there is no second opinion to be wrong about.
    Unreported,
    /// Disagreeing sizes. Callers take the non-destructive branch; we do not guess via ppid.
    Ambiguous,
}

pub(super) fn box_output_size(scope: Scope<'_>) -> BoxOutputSize {
    classify_output_size(&scoped_argvs(scope))
}

/// Agreed size, or `None`. Sound only for the libei hint (unknown → raw client pixels). Anything
/// that would act on Unreported vs Ambiguous must call [`box_output_size`].
pub(super) fn current_gamescope_output_size(scope: Scope<'_>) -> Option<(u32, u32)> {
    match box_output_size(scope) {
        BoxOutputSize::Known(size) => Some(size),
        BoxOutputSize::Unreported | BoxOutputSize::Ambiguous => None,
    }
}

fn classify_output_size(argvs: &[Vec<String>]) -> BoxOutputSize {
    let mut agreed: Option<(u32, u32)> = None;
    for argv in argvs {
        let Some(size) = gamescope_output_size(argv) else {
            continue;
        };
        match agreed {
            None => agreed = Some(size),
            Some(seen) if seen == size => {}
            Some(seen) => {
                tracing::debug!(
                    ?seen,
                    ?size,
                    "gamescope: two coexisting gamescopes report different output sizes — \
                     answering 'ambiguous' rather than picking one of them"
                );
                return BoxOutputSize::Ambiguous;
            }
        }
    }
    match agreed {
        Some(size) => BoxOutputSize::Known(size),
        None => BoxOutputSize::Unreported,
    }
}

/// After a restart: did `target` come up? Unanimity is wrong — a kept bare spawn at another size
/// would hold `Ambiguous` for the whole wait.
pub(super) fn any_output_size_is(argvs: &[Vec<String>], target: (u32, u32)) -> bool {
    argvs
        .iter()
        .any(|argv| gamescope_output_size(argv) == Some(target))
}

/// Headless `--nested-refresh` is the session's only refresh (defaults to 60 Hz). The wrapper can
/// lose it; refusing would loop (same env). Warn and carry on. Silent when `/proc` cannot be read.
pub(super) fn warn_if_mode_lost(mode: Mode, want_hz: u32, scope: Scope<'_>) {
    let argvs = scoped_argvs(scope);
    let lost = mode_mismatch(mode.width, mode.height, want_hz, &argvs);
    if lost.is_empty() {
        return;
    }
    tracing::warn!(
        lost = %lost.join(", "),
        "gamescope: the session did not start at the mode we asked for — the session script \
         dropped GAMESCOPE_BIN / SCREEN_WIDTH / SCREEN_HEIGHT. A headless gamescope reports \
         `--nested-refresh` as its ONE refresh rate (60 Hz when the flag never arrives), so games \
         and Steam will believe the display runs at that rate however fast the stream is. Install \
         punktfunk-gamescope, or check /etc/gamescope-session-plus/sessions.d/ for a file that \
         overrides GAMESCOPE_BIN or sets GAMESCOPECMD"
    );
}

/// Fail-open like [`missing_flags`]: empty argvs means we could not look.
fn mode_mismatch(want_w: u32, want_h: u32, want_hz: u32, argvs: &[Vec<String>]) -> Vec<String> {
    if argvs.is_empty() {
        return Vec::new();
    }
    let mut lost = Vec::new();
    let sizes: Vec<(u32, u32)> = argvs
        .iter()
        .filter_map(|a| {
            Some((
                argv_u32(a, &["-W", "--output-width"])?,
                argv_u32(a, &["-H", "--output-height"])?,
            ))
        })
        .collect();
    // No output size at all: cannot tell ours from a nested one — stay quiet.
    if !sizes.is_empty() && !sizes.contains(&(want_w, want_h)) {
        lost.push(format!(
            "resolution asked={want_w}x{want_h}, got={}",
            sizes
                .iter()
                .map(|(w, h)| format!("{w}x{h}"))
                .collect::<Vec<_>>()
                .join("/")
        ));
    }
    let rates: Vec<u32> = argvs
        .iter()
        .filter_map(|a| argv_u32(a, &["-r", "--nested-refresh"]))
        .collect();
    if !rates.contains(&want_hz) {
        lost.push(match rates.as_slice() {
            [] => format!(
                "refresh asked={want_hz}Hz, got=no --nested-refresh at all (gamescope defaults to \
                 60Hz headless)"
            ),
            got => format!(
                "refresh asked={want_hz}Hz, got={}Hz",
                got.iter().map(u32::to_string).collect::<Vec<_>>().join("/")
            ),
        });
    }
    lost
}

/// Managed modes can lose flags (session script / PATH shim). A lost cursor flag is silent: the
/// host was told the compositor would paint the pointer. Latch off ([`note_spawn_flags_lost`]) and
/// refuse; the retry plans host-composited SDR. Fail open if we cannot look. Any one gamescope
/// carrying the flags is enough — demanding every one would reject a good session beside a nested.
pub(super) fn verify_managed_spawn_flags(hdr: bool, scope: Scope<'_>) -> Result<()> {
    // The rate is a placeholder: only flag NAMES are kept, and `--adaptive-sync` is what proves
    // the VRR half of the plan reached the compositor.
    let expected: Vec<String> = our_flags(hdr, 1)
        .into_iter()
        .filter(|a| a.starts_with("--")) // flag names only — their values are bare words
        .collect();
    if expected.is_empty() {
        return Ok(());
    }
    let missing = missing_flags(&expected, &scoped_argvs(scope));
    if missing.is_empty() {
        tracing::debug!(flags = ?expected, "gamescope: the session's compositor carries our flags");
        return Ok(());
    }
    note_spawn_flags_lost();
    // Warn as well as erroring: the latch is a process-wide capability change, and whichever
    // caller consumes this error decides on its own how loudly to report it.
    tracing::warn!(
        missing = %missing.join(" "),
        "gamescope: the session ignored GAMESCOPE_BIN / the PATH shim and ran a stock gamescope — \
         HDR and the in-node cursor are now off for this host process"
    );
    Err(anyhow!(
        "the gamescope session started without {} — it ignored GAMESCOPE_BIN / the PATH shim and \
         ran a stock gamescope. Refusing it rather than streaming a session whose shape was \
         planned around flags that never arrived (a missing cursor flag has no symptom but an \
         absent pointer). Those capabilities are off for this host now; reconnect for a plain SDR \
         session, or install punktfunk-gamescope as the box's `gamescope`",
        missing.join(" ")
    ))
}

/// Empty `argvs` = could not look (silence). Empty result after looking = fine. Opposite meanings.
fn missing_flags<'a>(expected: &'a [String], argvs: &[Vec<String>]) -> Vec<&'a str> {
    if argvs.is_empty() {
        return Vec::new();
    }
    expected
        .iter()
        .filter(|f| !argvs.iter().any(|argv| argv.iter().any(|a| a == *f)))
        .map(String::as_str)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    /// Two gamescopes disagree: "cannot tell" is handleable; a confident wrong number is not.
    #[test]
    fn the_output_size_probe_refuses_to_pick_between_disagreeing_gamescopes() {
        let session = argv("/usr/bin/gamescope -W 1920 -H 1080 --prefer-output HDMI-A-1");
        let nested = argv("gamescope --backend wayland -W 1280 -H 800");
        // One compositor, or several that agree — a plain answer.
        assert_eq!(
            classify_output_size(std::slice::from_ref(&session)),
            BoxOutputSize::Known((1920, 1080))
        );
        assert_eq!(
            classify_output_size(&[session.clone(), session.clone()]),
            BoxOutputSize::Known((1920, 1080))
        );
        // Disagreement is AMBIGUOUS, not a coin flip — in either enumeration order.
        assert_eq!(
            classify_output_size(&[session.clone(), nested.clone()]),
            BoxOutputSize::Ambiguous
        );
        assert_eq!(
            classify_output_size(&[nested.clone(), session.clone()]),
            BoxOutputSize::Ambiguous
        );
        // Unreported ≠ Ambiguous: re-mode on the first, never the second.
        assert_eq!(classify_output_size(&[]), BoxOutputSize::Unreported);
        assert_eq!(
            classify_output_size(&[argv("gamescope --steam")]),
            BoxOutputSize::Unreported
        );
        assert_ne!(BoxOutputSize::Unreported, BoxOutputSize::Ambiguous);
    }

    /// After restart: is `target` present? Unanimity would hold Ambiguous for a kept stray spawn.
    #[test]
    fn the_post_restart_wait_asks_whether_the_target_size_is_present() {
        let session = argv("/usr/bin/gamescope -W 1920 -H 1080");
        let stray = argv("punktfunk-gamescope --backend headless -W 1280 -H 720");
        assert!(any_output_size_is(
            std::slice::from_ref(&session),
            (1920, 1080)
        ));
        // The stray one neither satisfies nor blocks the answer.
        assert!(any_output_size_is(
            &[stray.clone(), session.clone()],
            (1920, 1080)
        ));
        assert!(!any_output_size_is(
            std::slice::from_ref(&stray),
            (1920, 1080)
        ));
        assert!(!any_output_size_is(&[], (1920, 1080)));
        // Unanimity would have blocked it — the regression this predicate replaces.
        assert_eq!(
            classify_output_size(&[stray, session]),
            BoxOutputSize::Ambiguous
        );
    }

    /// `-W`/`-H` must be read as a pair off ONE argv, and the long spellings count: a half-answer
    /// would otherwise be published as a monitor row (`heads_under`) or a pointer scale.
    #[test]
    fn output_size_needs_both_flags_from_the_same_argv() {
        assert_eq!(
            gamescope_output_size(&argv("gamescope -W 2560 -H 1440")),
            Some((2560, 1440))
        );
        assert_eq!(
            gamescope_output_size(&argv("gamescope --output-width 800 --output-height 600")),
            Some((800, 600))
        );
        // getopt_long also takes `--flag=value`, the spelling `heads` already reads.
        assert_eq!(
            gamescope_output_size(&argv("gamescope --output-width=800 --output-height=600")),
            Some((800, 600))
        );
        assert_eq!(gamescope_output_size(&argv("gamescope -W 2560")), None);
        assert_eq!(gamescope_output_size(&argv("gamescope -H 1440")), None);
        // The NESTED size (`-w`/`-h`) is a different thing and must never stand in for the output.
        assert_eq!(
            gamescope_output_size(&argv("gamescope -w 1280 -h 800")),
            None
        );
    }

    /// A headless gamescope reports `--nested-refresh` as its one refresh rate and falls back to
    /// 60 Hz when the flag never arrives, so a session that lost the `GAMESCOPE_BIN` wrapper
    /// streams at the client's rate while telling every game it is 60.
    #[test]
    fn mode_mismatch_names_what_the_session_actually_got() {
        let argv = |s: &str| -> Vec<String> { s.split(' ').map(str::to_string).collect() };

        // The good case: our own managed spawn, carrying everything we asked for.
        let ok = vec![argv(
            "/usr/bin/gamescope --backend headless -W 1920 -H 1080 --nested-refresh 120 --steam",
        )];
        assert!(mode_mismatch(1920, 1080, 120, &ok).is_empty());

        // Wrapper dropped: no `--nested-refresh` anywhere and gamescope silently ran its 60 Hz
        // default. Size still landed (SCREEN_WIDTH survived).
        let lost = vec![argv(
            "/usr/bin/gamescope --backend headless -W 1920 -H 1080 --steam",
        )];
        let got = mode_mismatch(1920, 1080, 120, &lost);
        assert_eq!(got.len(), 1, "only the refresh is wrong: {got:?}");
        assert!(got[0].contains("asked=120Hz"), "{got:?}");
        assert!(got[0].contains("no --nested-refresh at all"), "{got:?}");

        // A wrong rate is reported with the number it actually got, not just "missing".
        let wrong = vec![argv("gamescope -W 1920 -H 1080 --nested-refresh 60")];
        let got = mode_mismatch(1920, 1080, 120, &wrong);
        assert_eq!(got.len(), 1);
        assert!(got[0].contains("got=60Hz"), "{got:?}");

        // Resolution lost too (SCREEN_WIDTH/HEIGHT dropped as well) — both are named.
        let both = vec![argv("gamescope -W 1280 -H 720")];
        assert_eq!(mode_mismatch(1920, 1080, 120, &both).len(), 2);

        // Fail open, exactly like `missing_flags`: nothing to compare against says nothing. A box
        // with a second gamescope that carries no output size must not produce a false alarm.
        assert!(mode_mismatch(1920, 1080, 120, &[]).is_empty());

        // Any running gamescope carrying the mode satisfies it — a Deck commonly runs a nested one
        // beside the session, and demanding that every gamescope match would reject a good session.
        let two = vec![
            argv("gamescope -W 1280 -H 800 --nested-refresh 60"),
            argv("gamescope -W 1920 -H 1080 --nested-refresh 120"),
        ];
        assert!(mode_mismatch(1920, 1080, 120, &two).is_empty());

        // The long spellings are read too.
        let long = vec![argv(
            "gamescope --output-width 1920 --output-height 1080 --nested-refresh 120",
        )];
        assert!(mode_mismatch(1920, 1080, 120, &long).is_empty());

        // A flag with no value after it must not panic or read past the end.
        let truncated = vec![argv("gamescope -W 1920 -H 1080 --nested-refresh")];
        assert_eq!(mode_mismatch(1920, 1080, 120, &truncated).len(), 1);
    }

    /// A managed session that ignored `GAMESCOPE_BIN` / the PATH shim runs a stock gamescope, and
    /// the host — already told the compositor would paint the pointer — paints none either. Only a
    /// compositor we can see, missing a flag we can name, may fail.
    #[test]
    fn spawn_flag_verification_fails_closed_only_on_evidence() {
        let argv = |s: &str| -> Vec<String> { s.split(' ').map(str::to_string).collect() };
        let want: Vec<String> = ["--hdr-enabled", "--pipewire-composite-cursor"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        // The flags arrived: nothing to report.
        assert!(missing_flags(
            &want,
            &[argv(
                "/usr/bin/punktfunk-gamescope --backend headless -W 1920 -H 1080 \
                 --hdr-enabled --hdr-debug-force-support --pipewire-composite-cursor"
            )]
        )
        .is_empty());

        // Distro binary: both flags lost. A lost cursor flag is silent.
        assert_eq!(
            missing_flags(
                &want,
                &[argv(
                    "/usr/bin/gamescope --backend headless -W 1920 -H 1080"
                )]
            ),
            vec!["--hdr-enabled", "--pipewire-composite-cursor"]
        );

        // A stock gamescope can take `--hdr-enabled` (it predates our patches) — so the HDR flag
        // alone proves nothing, and the cursor flag must be checked on its own.
        assert_eq!(
            missing_flags(
                &want,
                &[argv("/usr/bin/gamescope --hdr-enabled -W 1920 -H 1080")]
            ),
            vec!["--pipewire-composite-cursor"]
        );

        // Fail open when we could not look: an unreadable `/proc` is not evidence of anything, and
        // treating it as a miss would fail every managed session on a hardened box.
        assert!(missing_flags(&want, &[]).is_empty());

        // Several gamescopes running (a nested game under the session): the flags need only be on
        // one of them — the session compositor.
        assert!(missing_flags(
            &want,
            &[
                argv("/usr/bin/gamescope -W 800 -H 600"),
                argv("/usr/bin/punktfunk-gamescope --hdr-enabled --pipewire-composite-cursor"),
            ]
        )
        .is_empty());
    }
}
