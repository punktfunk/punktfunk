//! The prompt before a dial to a profile on a person's own account, which asks for its
//! password, or to a locked profile, which asks for its PIN; with Windows Hello beside it: a
//! Hello proof when this device holds the profile's key, a new key from the prompt's switch.
//! The rules are `pf_client_core::unlock`; this file runs them off the UI thread and draws the
//! prompt as a root sheet, like the picker.

use super::style::*;
use super::{AppCtx, AsyncSetState, Screen, Target};
use crate::hello::{self, Hello};
use pf_client_core::library::HostApi;
use pf_client_core::profiles::{ListedProfile, UnlockNeed};
use pf_client_core::unlock::{self, DeviceKey, Gate};
use pf_fluent::Palette;
use pf_l10n::{client, common};
use punktfunk_core::quic::v2::msg::{ProfileUnlock, Zeroizing};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use windows_reactor::*;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// What a dial does with the unlock it gets: `None` dials without one.
type Go = Box<dyn FnOnce(Option<ProfileUnlock>) + Send>;
/// What Sign in does with the typed password and the switch.
type Submit = Arc<Mutex<Option<Box<dyn FnOnce(Zeroizing<Vec<u8>>, bool) + Send>>>>;

/// One open prompt. Root state, so equality is the ask's identity.
#[derive(Clone)]
pub(crate) struct UnlockAsk {
    id: u64,
    name: String,
    host: String,
    /// The host's sentence, or why the typed password can't be sent.
    error: Option<String>,
    /// The switch enrols a Windows Hello key instead of staying signed in.
    hello: bool,
    /// It asks for the profile's PIN, not an account password.
    pin: bool,
    /// The typed password or PIN: the one copy this shell keeps, in a buffer that wipes.
    password: Arc<Mutex<Zeroizing<Vec<u8>>>>,
    on: Arc<AtomicBool>,
    submit: Submit,
    /// Cancel returns to the host list.
    back: AsyncSetState<Screen>,
}

impl PartialEq for UnlockAsk {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl UnlockAsk {
    /// Close without signing in: nothing dials, and the typed password goes.
    pub(crate) fn abandon(&self) {
        *self.password.lock().unwrap() = Zeroizing::new(Vec::new());
        self.submit.lock().unwrap().take();
        self.back.call(Screen::Hosts);
    }
}

/// Before the dial of `row`: no unlock, a Hello proof, or the prompt, then `go` with what it
/// produced. A profile that needs nothing dials on the calling thread; the rest runs on its
/// own, since Hello blocks.
pub(crate) fn before_dial(
    ctx: &Arc<AppCtx>,
    target: Target,
    pin: Option<[u8; 32]>,
    row: ListedProfile,
    set_screen: &AsyncSetState<Screen>,
    go: impl FnOnce(Option<ProfileUnlock>) + Send + 'static,
) {
    // An unpinned dial can't carry an unlock; the host's refusal says what to do.
    let Some(box_fp) = pin.filter(|_| row.unlock != UnlockNeed::None) else {
        return go(None);
    };
    let (ctx, set_screen) = (ctx.clone(), set_screen.clone());
    let _ = std::thread::Builder::new()
        .name("pf-unlock".into())
        .spawn(move || {
            let hello = hello::supported();
            let keys = hello.then_some(Hello);
            let key = keys
                .as_ref()
                .and_then(|h| DeviceKey::new(h, &box_fp, &row.id));
            let gate = unlock::gate(row.unlock, key.as_ref());
            let step = Step::new(&ctx, target, box_fp, row, hello, &set_screen);
            step.run(gate, key.as_ref(), Box::new(go));
        });
}

/// The host closed profile `id`'s dial with `ProfileLocked`; `sent`: it carried an unlock.
/// Its list is read again for the row, then [`unlock::after_refusal`] picks the prompt, with
/// `error`, or a Hello proof. A profile it no longer lists leaves the refusal on the status
/// line.
#[allow(clippy::too_many_arguments)]
pub(crate) fn after_refusal(
    ctx: &Arc<AppCtx>,
    target: Target,
    pin: Option<[u8; 32]>,
    id: String,
    error: Option<String>,
    sent: bool,
    set_screen: &AsyncSetState<Screen>,
    set_status: &AsyncSetState<String>,
    go: impl FnOnce(Option<ProfileUnlock>) + Send + 'static,
) {
    let (ctx, ss, st) = (ctx.clone(), set_screen.clone(), set_status.clone());
    let _ = std::thread::Builder::new()
        .name("pf-unlock".into())
        .spawn(move || {
            let row = super::profiles::relisted(&ctx, &target, pin, &id);
            let (Some(row), Some(box_fp)) = (row, pin) else {
                st.call(error.unwrap_or_else(|| {
                    pf_client_core::trust::connect_reject_message(
                        punktfunk_core::reject::RejectReason::ProfileLocked,
                    )
                }));
                return ss.call(Screen::Hosts);
            };
            let hello = hello::supported();
            let keys = hello.then_some(Hello);
            let key = keys
                .as_ref()
                .and_then(|h| DeviceKey::new(h, &box_fp, &row.id));
            let gate = unlock::after_refusal(row.unlock, key.as_ref(), sent, error);
            Step::new(&ctx, target, box_fp, row, hello, &ss).run(gate, key.as_ref(), Box::new(go));
        });
}

/// One profile's unlock on a worker thread, from its gate to the dial.
struct Step {
    ctx: Arc<AppCtx>,
    target: Target,
    box_fp: [u8; 32],
    row: ListedProfile,
    hello: bool,
    set_screen: AsyncSetState<Screen>,
}

impl Step {
    fn new(
        ctx: &Arc<AppCtx>,
        target: Target,
        box_fp: [u8; 32],
        row: ListedProfile,
        hello: bool,
        set_screen: &AsyncSetState<Screen>,
    ) -> Self {
        let (ctx, set_screen) = (ctx.clone(), set_screen.clone());
        Step {
            ctx,
            target,
            box_fp,
            row,
            hello,
            set_screen,
        }
    }

    /// `gate`, then `go`: a Hello proof dials, one that fails opens the prompt, the prompt
    /// opens with its error, and [`Gate::Dial`] dials with nothing.
    fn run(self, gate: Gate, key: Option<&DeviceKey>, go: Go) {
        let gate = match (gate, key) {
            (Gate::Key, Some(k)) => {
                let api = api(&self.ctx, &self.target, self.box_fp);
                match unlock::key_unlock(&api, &self.row.id, k) {
                    Some(proof) => return go(Some(proof)),
                    None => unlock::after_key_failure(),
                }
            }
            (Gate::Key, None) => unlock::after_key_failure(),
            (gate, _) => gate,
        };
        let Step {
            ctx,
            target,
            box_fp,
            row,
            hello,
            set_screen,
        } = self;
        match gate {
            Gate::Prompt { error } => {
                prompt(&ctx, target, box_fp, row, hello, error, &set_screen, go)
            }
            Gate::Dial | Gate::Key => go(None),
        }
    }
}

/// `target`'s management API, pinned to the box the proof names.
fn api(ctx: &AppCtx, target: &Target, box_fp: [u8; 32]) -> HostApi {
    let mgmt = target
        .host
        .mgmt_port
        .unwrap_or(pf_client_core::library::DEFAULT_MGMT_PORT);
    let (addr, port) = (&target.host.addr, target.host.port);
    HostApi::new(addr, mgmt, port, &ctx.identity, Some(box_fp))
}

/// Raises the prompt for `row`, its PIN form for a locked profile. Sign in builds the unlock on
/// a worker (a new Hello key shows Hello's own prompt) and hands it to `go`, and what was typed
/// is wiped as the worker ends; a password or PIN the wire refuses reopens the prompt.
#[allow(clippy::too_many_arguments)]
fn prompt(
    ctx: &Arc<AppCtx>,
    target: Target,
    box_fp: [u8; 32],
    row: ListedProfile,
    hello: bool,
    error: Option<String>,
    set_screen: &AsyncSetState<Screen>,
    go: Go,
) {
    let Some(set) = ctx.shared.set_unlock.lock().unwrap().clone() else {
        return set_screen.call(Screen::Hosts);
    };
    let (name, host) = (row.display_name.clone(), target.host.name.clone());
    let pin = row.unlock.pin();
    let (ctx2, ss) = (ctx.clone(), set_screen.clone());
    let submit = move |typed: Zeroizing<Vec<u8>>, on: bool| {
        let _ = std::thread::Builder::new()
            .name("pf-unlock".into())
            .spawn(move || {
                let keys = hello.then_some(Hello);
                let key = keys
                    .as_ref()
                    .and_then(|h| DeviceKey::new(h, &box_fp, &row.id));
                let made = match pin {
                    true => unlock::pin_unlock(&typed, on, key.as_ref()),
                    false => unlock::prompt_unlock(typed, on, key.as_ref()),
                };
                match made {
                    Ok(u) => go(Some(u)),
                    Err(line) => {
                        let error = Some(line.to_string());
                        prompt(&ctx2, target, box_fp, row, hello, error, &ss, go)
                    }
                }
            });
    };
    set.call(Some(UnlockAsk {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        name,
        host,
        error,
        hello,
        pin,
        password: Arc::new(Mutex::new(Zeroizing::new(Vec::new()))),
        on: Arc::new(AtomicBool::new(false)),
        submit: Arc::new(Mutex::new(Some(Box::new(submit)))),
        back: set_screen.clone(),
    }));
}

/// The prompt overlay, in its own root slot: the profile's name, the host's sentence, the
/// masked field for the password or the PIN, the one switch (off), Cancel and Sign in.
pub(crate) fn unlock_slot(
    pal: &Palette,
    ask: &Option<UnlockAsk>,
    set_unlock: &AsyncSetState<Option<UnlockAsk>>,
) -> View {
    let Some(a) = ask else {
        return Border::new().into();
    };
    let intro = match a.pin {
        true => client::unlock_pin_intro(&a.host),
        false => client::unlock_intro(&a.host),
    };
    let mut body: Vec<View> = vec![
        text(client::unlock_title(&a.name))
            .font_size(20.0)
            .font_weight(FontWeight::BOLD)
            .text_wrapping(TextWrapping::Wrap)
            .into(),
        muted(pal, intro)
            .font_size(13.0)
            .text_wrapping(TextWrapping::Wrap)
            .into(),
    ];
    if let Some(error) = &a.error {
        body.push(
            InfoBar::new()
                .message(error.as_str())
                .severity(InfoBarSeverity::Error)
                .is_open(true)
                .is_closable(false)
                .into(),
        );
    }
    // Each change replaces the copy, and the replaced one wipes as it drops.
    let typed = a.password.clone();
    let field = match a.pin {
        true => PasswordBox::new()
            .header(client::unlock_pin())
            .placeholder_text(client::unlock_pin_placeholder()),
        false => PasswordBox::new().header(client::unlock_password()),
    };
    body.push(
        field
            .password_reveal_mode(PasswordRevealMode::Peek)
            .on_password_changed(move |p: Rc<str>| {
                *typed.lock().unwrap() = Zeroizing::new(p.as_bytes().to_vec());
            })
            .into(),
    );
    let on = a.on.clone();
    body.push(
        ToggleSwitch::new()
            .is_on(false)
            .header(if a.hello {
                client::unlock_with_biometry("Windows Hello")
            } else {
                client::unlock_stay_signed_in().to_string()
            })
            .on_content(common::common_on())
            .off_content(common::common_off())
            .on_toggled(move |v: bool| on.store(v, Ordering::SeqCst))
            .into(),
    );
    let cancel = {
        let (set, a) = (set_unlock.clone(), a.clone());
        move || {
            set.call(None);
            a.abandon();
        }
    };
    let sign_in = {
        let (set, a) = (set_unlock.clone(), a.clone());
        move || {
            let password =
                std::mem::replace(&mut *a.password.lock().unwrap(), Zeroizing::new(Vec::new()));
            set.call(None);
            if let Some(f) = a.submit.lock().unwrap().take() {
                f(password, a.on.load(Ordering::SeqCst));
            }
        }
    };
    let click = cancel.clone();
    body.push(
        hstack((
            button(common::common_cancel()).on_click(click),
            button(client::unlock_sign_in())
                .style(ButtonStyle::Accent)
                .on_click(sign_in),
        ))
        .spacing(8.0)
        .horizontal_alignment(HorizontalAlignment::Right)
        .into(),
    );
    sheet(pal, vstack(body).spacing(14.0), 480.0, 1.0, cancel)
}
