//! What a session streams and how it holds its display. [`Intent`] is the mode a session means
//! to encode, the view it keeps and what its encoder frames for: settled at the Hello, moved by
//! the client's asks. [`Tenure`] is settled once at admission ([`Tenure::of`]): its own display,
//! one it joined, or a host monitor. [`Hold`] is whether another session is on that display now,
//! read from the display registry by generation, which comes in as an input: this module does no
//! I/O. The stream writes each display it adopts into a [`TenureCell`], which the control task
//! reads at each ask.
//!
//! Every mode, view and tenure question is a method here; a caller never re-derives one. Pinned
//! by `mode_view_vectors`, the `geometry_rows` and `tenure_rows`.

use crate::encode::Codec;
use punktfunk_core::video_fit::VideoFit;
use punktfunk_core::Mode;
use std::sync::atomic::{AtomicU64, Ordering};

/// The mode a session means to encode, the view the client shows it at, and what the encoder
/// frames the capture for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Intent {
    mode: Mode,
    /// Smaller than `mode` on one axis and inside it on both; `None` shows the mode.
    view: Option<(u32, u32)>,
    /// The view at Fit, a joiner's own view at its fit, or a mirrored head fitted to the mode
    /// the client asked for. `None` encodes the capture as it comes.
    framing: Option<(VideoFit, (u32, u32))>,
}

impl Intent {
    /// The Hello's intent at `asked`, the mode the session gets: the client's `view` kept by
    /// [`kept`]. A joiner's `asked` is the owner's mode, shown at the view and fit of its
    /// tenure; a mirror fits its head to `asked` and keeps no view.
    pub(crate) fn at_hello(
        asked: Mode,
        view: Option<(u32, u32)>,
        tenure: &Tenure,
        codec: Codec,
    ) -> Self {
        if let Some((fit, shown)) = tenure.framing() {
            return Intent {
                mode: asked,
                view: Some(shown),
                framing: Some((fit, shown)),
            };
        }
        if tenure.mirrors() {
            return Intent {
                mode: asked,
                view: None,
                framing: Some((VideoFit::Fit, (asked.width, asked.height))),
            };
        }
        Intent::fitted(asked, kept(view, &asked, codec))
    }

    /// A mode ask against this intent: the ask's own view, else this one while the size stays,
    /// kept by [`kept`] and framed at Fit.
    pub(crate) fn ask(&self, mode: Mode, view: Option<(u32, u32)>, codec: Codec) -> Self {
        let same_size = (mode.width, mode.height) == (self.mode.width, self.mode.height);
        let view = view.or(self.view.filter(|_| same_size));
        Intent::fitted(mode, kept(view, &mode, codec))
    }

    /// This ask on a display that runs `display` and stays: the asked size, or its view, framed
    /// on that mode while it is smaller on an axis. A size at least the display's frames
    /// nothing.
    pub(crate) fn on_display(&self, display: Mode) -> Self {
        if self.mode == display {
            return *self;
        }
        let (w, h) = self.view.unwrap_or((self.mode.width, self.mode.height));
        let view = (w < display.width || h < display.height).then_some((w, h));
        Intent::fitted(display, view)
    }

    /// This intent on a source that delivers `mode` with no ask: the view and framing stay.
    pub(crate) fn delivered_at(&self, mode: Mode) -> Self {
        Intent { mode, ..*self }
    }

    fn fitted(mode: Mode, view: Option<(u32, u32)>) -> Self {
        Intent {
            mode,
            view,
            framing: view.map(|v| (VideoFit::Fit, v)),
        }
    }

    pub(crate) fn mode(&self) -> Mode {
        self.mode
    }

    pub(crate) fn view(&self) -> Option<(u32, u32)> {
        self.view
    }

    /// What the encoder frames the capture for, into `open_encoder_fitted`.
    pub(crate) fn framing(&self) -> Option<(VideoFit, (u32, u32))> {
        self.framing
    }

    /// The mode at the size the client shows it: what a build is priced at before it opens.
    pub(crate) fn picture(&self) -> Picture {
        self.encoded(self.view.unwrap_or((self.mode.width, self.mode.height)))
    }

    /// The mode at the size an encoder opened for this intent took it to.
    pub(crate) fn encoded(&self, (width, height): (u32, u32)) -> Picture {
        Picture(Mode {
            width,
            height,
            ..self.mode
        })
    }
}

/// The size and rate a bitrate is priced at. Only an [`Intent`] makes one, so every price
/// follows from what the session means to encode or what its encoder took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Picture(Mode);

impl Picture {
    pub(crate) fn mode(&self) -> Mode {
        self.0
    }

    /// The picture as a `StreamConfig` or `ServerHello` names it beside `mode`: `None` at
    /// `mode`'s own size.
    pub(crate) fn wire(&self, mode: Mode) -> Option<(u32, u32)> {
        let size = (self.0.width, self.0.height);
        (size != (mode.width, mode.height)).then_some(size)
    }
}

/// `view` while it is a size `mode` can be scaled to: one the codec encodes, no larger than
/// `mode` on either axis and smaller on one. Anything else encodes the mode.
fn kept(view: Option<(u32, u32)>, mode: &Mode, codec: Codec) -> Option<(u32, u32)> {
    let (w, h) = view?;
    let kept = crate::encode::validate_dimensions(codec, w, h).is_ok()
        && w <= mode.width
        && h <= mode.height
        && (w < mode.width || h < mode.height);
    if !kept {
        tracing::debug!(view = ?(w, h), ?mode, "client view not kept — encoding the mode");
    }
    kept.then_some((w, h))
}

/// How a session holds the display it streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Tenure(Kind);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// A display of its own.
    Own,
    /// The display `mode_conflict: join` admitted it onto, shown at its own view and fit.
    Joiner { fit: VideoFit, view: (u32, u32) },
    /// A host monitor.
    Mirror,
}

/// Whether another session is on the display now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hold {
    Alone,
    /// Another session joined this session's display.
    Shared,
    /// A joined display that is not this session's to change: another holds it, its generation
    /// is not known yet, or the host is not Linux.
    Held,
}

impl Tenure {
    /// From the handshake: the fit and view a joiner shows the owner's display at, and whether
    /// the screen picked is a host monitor. A joiner picks no screen.
    pub(crate) fn of(joined: Option<(VideoFit, (u32, u32))>, mirrored: bool) -> Self {
        Tenure(match joined {
            Some((fit, view)) => Kind::Joiner { fit, view },
            None if mirrored => Kind::Mirror,
            None => Kind::Own,
        })
    }

    /// Who is on the display of `generation` now; `shared` answers whether another session
    /// holds a generation.
    pub(crate) fn hold(&self, generation: Option<u64>, shared: impl FnOnce(u64) -> bool) -> Hold {
        match (self.0, generation) {
            (Kind::Joiner { .. }, _) => joiner_hold(generation, shared),
            (_, Some(g)) if shared(g) => Hold::Shared,
            _ => Hold::Alone,
        }
    }

    /// The session takes another's capture: its cursor is a capture worker's, its rate and
    /// format are the display's.
    pub(crate) fn shares_capture(&self) -> bool {
        !matches!(self.0, Kind::Own)
    }

    /// Further screens are offered only to a session that builds displays of its own.
    pub(crate) fn offers_screens(&self) -> bool {
        matches!(self.0, Kind::Own)
    }

    /// A codec ask may rebuild the capture: not a mirror's. A joiner's is asked again at each
    /// change ([`Hold`]).
    pub(crate) fn may_change_format(&self) -> bool {
        !matches!(self.0, Kind::Mirror)
    }

    /// The display request shares the live display instead of creating one.
    pub(crate) fn join_live(&self) -> bool {
        matches!(self.0, Kind::Joiner { .. })
    }

    /// The session streams a host monitor, whose mode and rate are the monitor's.
    pub(crate) fn mirrors(&self) -> bool {
        matches!(self.0, Kind::Mirror)
    }

    /// The view and fit a joiner's encoder frames the shared picture for.
    pub(crate) fn framing(&self) -> Option<(VideoFit, (u32, u32))> {
        match self.0 {
            Kind::Joiner { fit, view } => Some((fit, view)),
            _ => None,
        }
    }

    /// A joiner alone on its display builds one of its own.
    pub(crate) fn stops_joining(&self, hold: Hold) -> bool {
        self.join_live() && hold == Hold::Alone
    }

    /// A content change rebuilds the capture in the content's range only where no other session
    /// takes that capture: not a joiner's, nor a display another is on.
    pub(crate) fn follows_content(&self, hold: Hold) -> bool {
        !self.join_live() && hold == Hold::Alone
    }

    /// The joiner built a display of its own and holds it as its owner.
    pub(crate) fn stop_joining(&mut self) {
        if self.join_live() {
            self.0 = Kind::Own;
        }
    }
}

/// A joiner's display is its own to change only alone on Linux, where it builds its own.
fn joiner_hold(generation: Option<u64>, shared: impl FnOnce(u64) -> bool) -> Hold {
    match generation {
        Some(g) if cfg!(target_os = "linux") && !shared(g) => Hold::Alone,
        _ => Hold::Held,
    }
}

/// The tenure of the display the stream last adopted, for the control task's asks.
pub(crate) struct TenureCell(AtomicU64);

/// [`TenureCell`]'s word for a display not joined; any other is a joiner's display generation.
const NOT_JOINED: u64 = 0;
/// [`TenureCell`]'s word for a joiner whose display generation is not known yet.
const UNSEEN: u64 = u64::MAX;

impl TenureCell {
    pub(crate) fn new(tenure: &Tenure) -> Self {
        let cell = TenureCell(AtomicU64::new(NOT_JOINED));
        cell.adopt(tenure, None);
        cell
    }

    /// The stream adopted a display of `generation`, held as `tenure`.
    pub(crate) fn adopt(&self, tenure: &Tenure, generation: Option<u64>) {
        let word = match tenure.0 {
            Kind::Joiner { .. } => generation.unwrap_or(UNSEEN),
            _ => NOT_JOINED,
        };
        self.0.store(word, Ordering::Relaxed);
    }

    /// The display is [`Hold::Held`]: the session may not change it now. `shared` is read only
    /// for a joiner's known generation on Linux.
    pub(crate) fn held(&self, shared: impl FnOnce(u64) -> bool) -> bool {
        match self.0.load(Ordering::Relaxed) {
            NOT_JOINED => false,
            UNSEEN => true,
            g => joiner_hold(Some(g), shared) == Hold::Held,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::native::stream::{tenure_plans, Build};
    use crate::vdisplay::Compositor;

    impl Intent {
        /// A fixture: `mode` framed as `framing`, its view the framed size.
        pub(crate) fn of(mode: Mode, framing: Option<(VideoFit, (u32, u32))>) -> Self {
            Intent {
                mode,
                view: framing.map(|(_, v)| v),
                framing,
            }
        }
    }

    impl Picture {
        /// A fixture: a price at `mode` as it stands.
        pub(crate) fn of(mode: Mode) -> Self {
            Picture(mode)
        }
    }

    /// The client's ask for `mode` at `view`, as the control task hands it on.
    pub(crate) fn asked(mode: Mode, view: Option<(u32, u32)>) -> Intent {
        Intent::fitted(mode, view)
    }

    /// An intent prices pixels at its view; without one, at the mode.
    #[test]
    fn an_intent_prices_the_picture_it_encodes() {
        let mode = Mode {
            width: 2048,
            height: 1152,
            refresh_hz: 120,
            refresh_mhz: 0,
        };
        assert_eq!(asked(mode, None).picture().mode(), mode);
        let p = asked(mode, Some((1366, 768))).picture().mode();
        assert_eq!((p.width, p.height, p.refresh_hz), (1366, 768, 120));
    }

    /// Who holds the display a session streams.
    #[derive(Clone, Copy, Debug)]
    pub(in crate::native) enum Case {
        /// Its own, alone.
        Own,
        /// Its own, which another session joined.
        OwnShared,
        /// One it joined, whose generation its stream has not seen.
        JoinerHeld,
        /// One it joined, which no other session holds now.
        JoinerAlone,
        /// A host monitor.
        Mirror,
    }

    /// A display generation no registry tracks.
    const UNTRACKED_GEN: u64 = 0x5EED_0007;

    impl Case {
        /// The case's tenure, the generation its stream adopted, and whether another session
        /// is on that display: the registry tracks none of the tests'.
        pub(in crate::native) fn held(self) -> (Tenure, Option<u64>, bool) {
            let own = Tenure::of(None, false);
            let joiner = Tenure::of(Some((VideoFit::Fit, (1920, 1080))), false);
            match self {
                Case::Own => (own, Some(UNTRACKED_GEN), false),
                Case::OwnShared => (own, Some(UNTRACKED_GEN), true),
                Case::JoinerHeld => (joiner, None, false),
                Case::JoinerAlone => (joiner, Some(UNTRACKED_GEN), false),
                Case::Mirror => (Tenure::of(None, true), Some(UNTRACKED_GEN), false),
            }
        }
    }

    /// A tenure's answers that sit behind the host OS.
    #[derive(Debug, PartialEq)]
    struct OsAnswers {
        /// The control task hands a mode switch on.
        resize: bool,
        /// The control task hands a codec change on.
        codec: bool,
        /// The plan of a larger mode: how it builds and whether it stops joining.
        larger: Option<(Build, bool)>,
        /// A fractional rate survives the Welcome, then the control task's asks.
        exact: (bool, bool),
        /// The cursor forwards as on a display of the session's own; `false`: never.
        cursor_as_own: bool,
        capture: crate::session_plan::CaptureBackend,
    }

    /// Every host's answers: a content change rebuilds, the format rule runs again when the
    /// source moves, `join_live`, further screens are offered, the tenure follows content.
    type Portable = (bool, bool, bool, bool, bool);

    /// One tenure: the answers every host gives, then each OS's.
    struct TenureRow {
        case: Case,
        portable: Portable,
        linux: OsAnswers,
        windows: OsAnswers,
        verdict: crate::native::tests::Verdict,
    }

    #[rustfmt::skip]
    fn tenures() -> Vec<TenureRow> {
        use crate::native::tests::Verdict::Wanted;
        use crate::session_plan::CaptureBackend::{IddPush, Portal, Wgc};
        use Case::*;
        let os = |resize, codec, larger, exact, cursor_as_own, capture| OsAnswers {
            resize, codec, larger, exact, cursor_as_own, capture,
        };
        let row = |case, portable, linux, windows, verdict| TenureRow {
            case, portable, linux, windows, verdict,
        };
        let once = || Some((Build::Once, false));
        vec![
            row(Own, (true, true, false, true, true),
                os(true, true, once(), (true, true), true, Portal),
                os(true, true, once(), (false, false), true, IddPush), Wanted),
            row(OwnShared, (false, true, false, true, false),
                os(true, true, None, (true, true), true, Portal),
                os(true, true, None, (false, false), true, IddPush), Wanted),
            row(JoinerHeld, (false, false, true, false, false),
                os(false, false, None, (false, true), true, Portal),
                os(false, false, None, (false, false), false, Wgc), Wanted),
            row(JoinerAlone, (false, false, true, false, false),
                os(true, true, Some((Build::Once, true)), (false, true), true, Portal),
                os(false, false, None, (false, false), false, Wgc), Wanted),
            row(Mirror, (true, false, false, false, true),
                os(false, false, once(), (false, false), true, Portal),
                os(false, false, once(), (false, false), false, Wgc), Wanted),
        ]
    }

    /// `case`'s answers on this host. The control task's gates compose as `native.rs` sets
    /// them and `inbound.rs` reads them, off the tenure cell the stream wrote.
    fn tenure_answers(case: Case) -> (Portable, OsAnswers) {
        use crate::encode::Codec;
        use crate::native::handshake::tests::{hello_at, live_display, mode};
        use crate::native::handshake::{cursor_forward, follows_source, runs_exact_rate};
        use punktfunk_core::quic::v2::{features::FeatureSet, registry::FEATURE_FORMAT_EPOCH};
        let (tenure, generation, shared) = case.held();
        let registry = |g| shared || crate::vdisplay::registry::shared(g);
        let cell = TenureCell::new(&tenure);
        cell.adopt(&tenure, generation);
        let held = cell.held(crate::vdisplay::registry::shared);
        let (content, larger, capture) = tenure_plans(case);
        let (encoder, knobs) = (
            crate::encode::resolve(),
            crate::native::SessionKnobs::read(),
        );
        let compositor = Some(Compositor::Mutter);
        let owner = tenure.join_live().then(live_display);
        let features = FeatureSet::default().with(FEATURE_FORMAT_EPOCH);
        let follows = crate::native::handshake::FormatFacts::of(
            &hello_at(mode(16, 16, 60)),
            Default::default(),
            compositor,
            None,
            &knobs,
            owner.as_ref(),
            follows_source(&features, &tenure),
        )
        .follows;
        let cursor = |worker| {
            let caps = punktfunk_core::quic::CLIENT_CAP_CURSOR;
            cursor_forward(caps, compositor, Codec::H265, 8, false, worker, &encoder)
        };
        let worker = tenure.shares_capture();
        let cursor_as_own = if cfg!(target_os = "linux") {
            cursor(worker) == cursor(false)
        } else {
            !worker || cursor(worker)
        };
        let hold = tenure.hold(generation, registry);
        let portable = (
            content,
            follows,
            tenure.join_live(),
            tenure.offers_screens(),
            tenure.follows_content(hold),
        );
        let resize = crate::native::stream::reconfig_allowed(compositor, false, tenure.mirrors());
        let os = OsAnswers {
            resize: resize && !held,
            codec: tenure.may_change_format() && !held,
            larger,
            exact: (
                runs_exact_rate(compositor, tenure.shares_capture()),
                runs_exact_rate(compositor, tenure.mirrors()),
            ),
            cursor_as_own,
            capture,
        };
        (portable, os)
    }

    /// Each tenure's answers: whether it may resize, change codec or follow content, whether
    /// its format follows the source, whether it is offered further screens, its cursor and
    /// its capture. Every host checks the portable ones; Linux and Windows each check theirs.
    #[test]
    fn tenure_rows() {
        for r in tenures() {
            let name = format!("{:?} ({:?})", r.case, r.verdict);
            let (portable, os) = tenure_answers(r.case);
            assert_eq!(portable, r.portable, "{name}");
            if cfg!(target_os = "linux") {
                assert_eq!(os, r.linux, "{name} on Linux");
            } else if cfg!(target_os = "windows") {
                assert_eq!(os, r.windows, "{name} on Windows");
            }
        }
    }
}
